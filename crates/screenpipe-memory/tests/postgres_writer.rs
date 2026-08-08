use std::env;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGapSummary, HashLedger, MERGE_CONTRACT_VERSION,
    MergeDecisionKind, ObservationSample, OpenEvent, PgEventReader, PgEventWriter, SplitReason,
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool, Row};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(1);

const AUTHORITATIVE_SCHEMA: &str = r#"
BEGIN;
CREATE TABLE machines (
    id BIGSERIAL PRIMARY KEY,
    slug TEXT NOT NULL,
    display_name TEXT NOT NULL,
    next_event_seq BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT machines_slug_format CHECK (slug ~ '^[a-z][a-z0-9_]*$'),
    CONSTRAINT machines_next_event_seq_positive CHECK (next_event_seq >= 1)
);
CREATE UNIQUE INDEX machines_slug_uidx ON machines (slug);
CREATE TABLE apps (
    id BIGSERIAL PRIMARY KEY,
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    app_key TEXT NOT NULL,
    app_title TEXT NOT NULL DEFAULT '',
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT apps_machine_app_key_uidx UNIQUE (machine_id, app_key)
);
CREATE INDEX apps_machine_id_idx ON apps (machine_id);
CREATE TABLE events (
    id TEXT PRIMARY KEY,
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    seq BIGINT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'screen',
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    app_id BIGINT REFERENCES apps (id),
    window_title TEXT NOT NULL DEFAULT '',
    ocr_text TEXT NOT NULL DEFAULT '',
    readable_text TEXT NOT NULL DEFAULT '',
    caption TEXT,
    title TEXT,
    ocr_text_hash TEXT NOT NULL DEFAULT '',
    sample_count INT NOT NULL DEFAULT 1,
    merge_meta JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    search_tsv TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A')
        || setweight(to_tsvector('english', coalesce(caption, '')), 'A')
        || setweight(to_tsvector('english', coalesce(readable_text, '')), 'B')
        || setweight(to_tsvector('english', coalesce(ocr_text, '')), 'C')
        || setweight(to_tsvector('english', coalesce(window_title, '')), 'D')
    ) STORED,
    CONSTRAINT events_machine_seq_uidx UNIQUE (machine_id, seq),
    CONSTRAINT events_seq_positive CHECK (seq >= 1),
    CONSTRAINT events_sample_count_positive CHECK (sample_count >= 1),
    CONSTRAINT events_window_order CHECK (ended_at >= started_at),
    CONSTRAINT events_kind_nonempty CHECK (kind <> '')
);
CREATE INDEX events_machine_started_idx ON events (machine_id, started_at DESC);
CREATE INDEX events_started_at_idx ON events (started_at DESC);
CREATE INDEX events_ocr_text_hash_idx ON events (ocr_text_hash) WHERE ocr_text_hash <> '';
CREATE INDEX events_app_id_idx ON events (app_id) WHERE app_id IS NOT NULL;
CREATE INDEX events_search_tsv_gin ON events USING GIN (search_tsv);
COMMIT;
"#;

const MALFORMED_WRITER_SCHEMA: &str = r#"
BEGIN;
CREATE TABLE machines (
    id BIGSERIAL PRIMARY KEY,
    slug TEXT NOT NULL,
    display_name TEXT NOT NULL,
    next_event_seq BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT machines_slug_format CHECK (slug ~ '^[a-z][a-z0-9_]*$'),
    CONSTRAINT machines_next_event_seq_positive CHECK (next_event_seq >= 1)
);
CREATE UNIQUE INDEX machines_slug_uidx ON machines (slug);
CREATE TABLE apps (id BIGINT PRIMARY KEY);
CREATE TABLE events (id TEXT PRIMARY KEY);
COMMIT;
"#;

struct TestDatabase {
    admin_pool: PgPool,
    pool: PgPool,
    scoped_url: String,
    schema: String,
}

/// The database name this suite is allowed to create and drop schemas in.
///
/// Deliberately an exact suffix rather than a substring: `screen_memory`
/// contains no `_test`, but a substring rule would also accept something like
/// `test_screen_memory_live`, and the point is to be unambiguous about which
/// database is disposable.
const TEST_DATABASE_SUFFIX: &str = "_test";

fn database_name(database_url: &str) -> Option<&str> {
    let after_scheme = database_url.split_once("://")?.1;
    let path = after_scheme.split_once('/')?.1;
    let name = path.split(['?', '#']).next()?;
    (!name.is_empty()).then_some(name)
}

fn ensure_test_database(database_url: &str) -> Result<()> {
    let name =
        database_name(database_url).context("SCREEN_MEMORY_DATABASE_URL has no database name")?;
    ensure!(
        name.ends_with(TEST_DATABASE_SUFFIX),
        "refusing to run schema-mutating tests against database {name:?}: \
         this suite issues CREATE SCHEMA and DROP SCHEMA CASCADE, so it will \
         only run against a database whose name ends in {TEST_DATABASE_SUFFIX:?}. \
         Point SCREEN_MEMORY_DATABASE_URL at {name}{TEST_DATABASE_SUFFIX}."
    );
    Ok(())
}

#[test]
fn the_production_capture_database_is_refused() {
    // The exact URL Doppler injects must be rejected. Without this the suite
    // silently ran DDL inside the live capture database.
    const CREDENTIAL_MARKER: &str = "__TEST_DATABASE_PASSWORD_MARKER__";
    let production =
        format!("postgresql://screen_memory:{CREDENTIAL_MARKER}@127.0.0.1:5432/screen_memory");
    let error = ensure_test_database(&production).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("refusing to run schema-mutating tests"),
        "expected a refusal, got: {rendered}"
    );
    // The refusal must not echo the credential it was handed.
    assert!(
        !rendered.contains(CREDENTIAL_MARKER),
        "the refusal leaked the test credential marker: {rendered}"
    );

    ensure_test_database("postgresql://u:p@127.0.0.1:5432/screen_memory_test")
        .expect("the test database must be accepted");
    ensure_test_database("postgresql://u:p@127.0.0.1:5432/screen_memory_test?sslmode=disable")
        .expect("query parameters must not defeat the check");
    // A name that merely CONTAINS the marker is not the test database.
    ensure_test_database("postgresql://u:p@127.0.0.1:5432/test_screen_memory")
        .expect_err("a substring match must not be accepted");
}

impl TestDatabase {
    async fn create() -> Result<Self> {
        Self::create_with_schema(AUTHORITATIVE_SCHEMA).await
    }

    async fn create_with_schema(schema_sql: &str) -> Result<Self> {
        let database_url = env::var("SCREEN_MEMORY_DATABASE_URL")
            .context("SCREEN_MEMORY_DATABASE_URL must be injected for PostgreSQL tests")?;
        // Refuse to run anywhere that is not obviously a test database.
        //
        // Doppler injects the PRODUCTION url, and the redirect to
        // screen_memory_test lived only in whatever shell command happened to
        // invoke cargo. Nothing stopped this suite from issuing CREATE SCHEMA
        // and DROP SCHEMA CASCADE inside the live capture database - and it
        // had already done so. The guard belongs here, next to the DDL, because
        // it must hold however the tests are invoked.
        ensure_test_database(&database_url)?;
        let schema = format!(
            "goal1_writer_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        let admin_pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .context("connect test schema administrator")?;
        admin_pool
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .context("create disposable PostgreSQL schema")?;
        let separator = if database_url.contains('?') { '&' } else { '?' };
        let scoped_url = format!("{database_url}{separator}options=-csearch_path%3D{schema}");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&scoped_url)
            .await
            .context("connect disposable PostgreSQL schema")?;
        sqlx::raw_sql(schema_sql)
            .execute(&pool)
            .await
            .context("apply authoritative schema in disposable search_path")?;
        Ok(Self {
            admin_pool,
            pool,
            scoped_url,
            schema,
        })
    }

    async fn cleanup(self) -> Result<()> {
        self.pool.close().await;
        self.admin_pool
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .context("drop disposable PostgreSQL schema")?;
        self.admin_pool.close().await;
        Ok(())
    }

    async fn finish(self, test_result: Result<()>) -> Result<()> {
        let cleanup_result = self.cleanup().await;
        match (test_result, cleanup_result) {
            (Ok(()), cleanup_result) => cleanup_result,
            (Err(test_error), Ok(())) => Err(test_error),
            (Err(test_error), Err(cleanup_error)) => Err(test_error.context(format!(
                "disposable-schema cleanup also failed: {cleanup_error:#}"
            ))),
        }
    }
}

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 4, 12, 0, second)
        .single()
        .unwrap()
}

fn event(
    second: u32,
    app_key: &str,
    app_title: &str,
    window_title: &str,
    text: &str,
    browser_url: Option<&str>,
) -> OpenEvent {
    let hash = format!("hash-{second}");
    OpenEvent {
        merge_contract_version: MERGE_CONTRACT_VERSION,
        started_at: at(0),
        ended_at: at(second),
        latest: ObservationSample {
            captured_at: at(second),
            app_key: app_key.to_owned(),
            app_title: app_title.to_owned(),
            window_title: window_title.to_owned(),
            ocr_text: text.to_owned(),
            readable_text: format!("readable {text}"),
            browser_url: browser_url.map(str::to_owned),
        },
        merge_hash: "stable-merge-hash".to_owned(),
        latest_exact_ocr_hash: hash.clone(),
        start_reason: SplitReason::Initial,
        last_decision: MergeDecisionKind::Start,
        latest_cadence: CadenceRecord {
            input: CadenceInput {
                input_idle: Duration::seconds(second.into()),
                frame_stable_for: Duration::seconds(second.into()),
                foreground_changed: false,
                frame_changed: second > 0,
            },
            next_interval: Duration::seconds(2),
        },
        capture_gaps: CaptureGapSummary {
            capture_unavailable: 1,
            ocr_unavailable: 2,
            empty_ocr: 3,
            // Non-zero on purpose: a fixture of 0 would let a writer that
            // hardcodes the key pass the exact-JSON assertion below.
            desktop_locked: 4,
        },
        sample_count: 1,
        hash_counts: HashLedger::from_hashes([hash]),
    }
}

async fn assert_preflight_rejects_single_mutation(mutation: &str, invariant: &str) -> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        sqlx::raw_sql(mutation).execute(&db.pool).await?;
        ensure!(
            writer.preflight().await.is_err(),
            "preflight accepted broken invariant: {invariant}"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn starts_allocate_icarus_ids_and_persist_authoritative_event_fields() -> Result<()> {
    let db = TestDatabase::create().await?;
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
    let first = event(0, "notepad.exe", "Notepad", "notes", "first text", None);
    let mut second = event(
        1,
        "msedge.exe",
        "Microsoft Edge",
        "browser",
        "second text",
        Some("https://example.test/path"),
    );
    // Deliberately not 1. `sample_count` has `DEFAULT 1` in the schema, so a
    // fixture of 1 is satisfied by the column default even if the writer never
    // binds the column at all.
    second.sample_count = 9;

    let first_id = writer.write_start(&first, SplitReason::Initial).await?;
    let second_id = writer.write_start(&second, SplitReason::AppChange).await?;
    let row = sqlx::query(
        "SELECT e.id, e.seq, e.kind, e.started_at, e.ended_at, e.window_title, e.ocr_text, \
                e.readable_text, e.ocr_text_hash, e.sample_count, e.merge_meta, e.title, \
                m.slug, m.display_name, a.app_key, a.app_title \
         FROM events e JOIN machines m ON m.id = e.machine_id \
         JOIN apps a ON a.id = e.app_id WHERE e.id = $1",
    )
    .bind(&second_id)
    .fetch_one(&db.pool)
    .await?;
    let meta: Value = row.try_get("merge_meta")?;

    ensure!((first_id, second_id.as_str()) == ("icarus_1".to_owned(), "icarus_2"));
    ensure!(row.try_get::<i64, _>("seq")? == 2);
    ensure!(row.try_get::<String, _>("kind")? == "screen");
    // The event window is the primary data product. Nothing read these two
    // columns back, so both could be bound from the same instant - collapsing
    // every event to zero duration - and the CHECK (ended_at >= started_at)
    // would still be satisfied.
    ensure!(row.try_get::<chrono::DateTime<Utc>, _>("started_at")? == at(0));
    ensure!(row.try_get::<chrono::DateTime<Utc>, _>("ended_at")? == at(1));
    ensure!(row.try_get::<String, _>("window_title")? == "browser");
    ensure!(row.try_get::<String, _>("ocr_text")? == "second text");
    ensure!(row.try_get::<String, _>("readable_text")? == "readable second text");
    ensure!(row.try_get::<String, _>("ocr_text_hash")? == "hash-1");
    // `title` is the weight-A branch of search_tsv and was NULL on every row
    // this system had ever written - 784 of 784 - so ranking could not tell the
    // window someone worked in from a word that happened to be on screen.
    // Nothing asserted it, so an INSERT that omitted the column entirely passed
    // the whole suite.
    ensure!(
        row.try_get::<Option<String>, _>("title")? == Some("Microsoft Edge - browser".to_owned()),
        "title was not persisted: {:?}",
        row.try_get::<Option<String>, _>("title")?
    );
    ensure!(row.try_get::<i32, _>("sample_count")? == 9);
    ensure!(row.try_get::<String, _>("slug")? == "icarus");
    ensure!(row.try_get::<String, _>("display_name")? == "Icarus-Laptop");
    ensure!(row.try_get::<String, _>("app_key")? == "msedge.exe");
    ensure!(row.try_get::<String, _>("app_title")? == "Microsoft Edge");

    // Assert the WHOLE merge_meta document, not a few keys. Cherry-picking left
    // merge_hash/latest_exact_ocr_hash swappable, the three capture-gap
    // counters swappable, and latest_cadence entirely droppable.
    ensure!(
        meta == json!({
            "merge_contract_version": MERGE_CONTRACT_VERSION,
            "start_reason": "app_change",
            "last_decision": "start",
            "merge_hash": "stable-merge-hash",
            "latest_exact_ocr_hash": "hash-1",
            "hashes_seen": { "hash-1": 1 },
            "hashes_seen_evicted": 0,
            "sample_count": 9,
            "latest_cadence": {
                "input_idle_ms": 1000,
                "frame_stable_for_ms": 1000,
                "foreground_changed": false,
                "frame_changed": true,
                "next_interval_ms": 2000,
            },
            "capture_gaps": {
                "capture_unavailable": 1,
                "ocr_unavailable": 2,
                "empty_ocr": 3,
                "desktop_locked": 4,
            },
            "browser_url": "https://example.test/path",
        }),
        "persisted merge_meta drifted: {meta:#}"
    );
    db.cleanup().await
}

#[tokio::test]
async fn concurrent_starts_allocate_each_machine_sequence_once() -> Result<()> {
    let db = TestDatabase::create().await?;
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
    let left = event(0, "notepad.exe", "Notepad", "left", "left text", None);
    let right = event(1, "notepad.exe", "Notepad", "right", "right text", None);

    let (left_id, right_id) = tokio::join!(
        writer.write_start(&left, SplitReason::Initial),
        writer.write_start(&right, SplitReason::WindowTitleChange)
    );
    let mut ids = vec![left_id?, right_id?];
    ids.sort();
    let next_event_seq: i64 =
        sqlx::query_scalar("SELECT next_event_seq FROM machines WHERE slug = 'icarus'")
            .fetch_one(&db.pool)
            .await?;
    ensure!(ids == ["icarus_1", "icarus_2"]);
    ensure!(next_event_seq == 3);
    db.cleanup().await
}

#[tokio::test]
async fn merge_refreshes_app_title_latest_text_and_domain_metadata() -> Result<()> {
    let db = TestDatabase::create().await?;
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
    let started = event(0, "msedge.exe", "Edge", "old title", "old text", None);
    let event_id = writer.write_start(&started, SplitReason::Initial).await?;
    let mut merged = event(
        5,
        "msedge.exe",
        "Microsoft Edge",
        "new title",
        "new text",
        Some("https://example.test/latest"),
    );
    merged.sample_count = 2;
    merged.last_decision = MergeDecisionKind::Merge;
    // Not `Initial`. `write_merge` passes `event.start_reason` through to
    // `merge_meta`; with an `Initial` fixture that argument could be hardcoded
    // and every merged row would claim it started for the wrong reason.
    merged.start_reason = SplitReason::AppChange;
    merged.hash_counts = HashLedger::from_hashes(["hash-0".to_owned(), "hash-5".to_owned()]);

    writer.write_merge(&event_id, &merged).await?;
    let row = sqlx::query(
        "SELECT e.started_at, e.ended_at, e.window_title, e.ocr_text, e.readable_text, \
                e.ocr_text_hash, e.sample_count, e.merge_meta, a.app_title \
         FROM events e JOIN apps a ON a.id = e.app_id WHERE e.id = $1",
    )
    .bind(&event_id)
    .fetch_one(&db.pool)
    .await?;
    let meta: Value = row.try_get("merge_meta")?;
    ensure!(row.try_get::<chrono::DateTime<Utc>, _>("started_at")? == at(0));
    ensure!(row.try_get::<chrono::DateTime<Utc>, _>("ended_at")? == at(5));
    ensure!(row.try_get::<String, _>("window_title")? == "new title");
    ensure!(row.try_get::<String, _>("ocr_text")? == "new text");
    ensure!(row.try_get::<String, _>("readable_text")? == "readable new text");
    ensure!(row.try_get::<String, _>("ocr_text_hash")? == "hash-5");
    ensure!(row.try_get::<i32, _>("sample_count")? == 2);
    ensure!(row.try_get::<String, _>("app_title")? == "Microsoft Edge");
    ensure!(
        meta == json!({
            "merge_contract_version": MERGE_CONTRACT_VERSION,
            "start_reason": "app_change",
            "last_decision": "merge",
            "merge_hash": "stable-merge-hash",
            "latest_exact_ocr_hash": "hash-5",
            "hashes_seen": { "hash-0": 1, "hash-5": 1 },
            "hashes_seen_evicted": 0,
            "sample_count": 2,
            "latest_cadence": {
                "input_idle_ms": 5000,
                "frame_stable_for_ms": 5000,
                "foreground_changed": false,
                "frame_changed": true,
                "next_interval_ms": 2000,
            },
            "capture_gaps": {
                "capture_unavailable": 1,
                "ocr_unavailable": 2,
                "empty_ocr": 3,
                "desktop_locked": 4,
            },
            "browser_url": "https://example.test/latest",
        }),
        "persisted merge_meta drifted: {meta:#}"
    );
    db.cleanup().await
}

#[tokio::test]
async fn preflight_rejects_removal_of_every_authoritative_constraint_and_index() -> Result<()> {
    // The constraint and index validators compare `count(matched)` against
    // `count(expected)`. Deleting a row from the expected VALUES list shrinks
    // BOTH sides, so the check still returns true - the expectation list can be
    // silently shortened. Only a per-object drift test closes that: if an
    // object is dropped from the expected list, the test that drops it from the
    // database stops failing and is itself detected.
    //
    // The objects below previously had no dedicated drift test at all.
    for (mutation, invariant) in [
        (
            "ALTER TABLE machines DROP CONSTRAINT machines_slug_format",
            "machines_slug_format",
        ),
        (
            "ALTER TABLE machines DROP CONSTRAINT machines_next_event_seq_positive",
            "machines_next_event_seq_positive",
        ),
        (
            "ALTER TABLE apps DROP CONSTRAINT apps_machine_id_fkey",
            "apps_machine_id_fkey",
        ),
        (
            "ALTER TABLE events DROP CONSTRAINT events_machine_id_fkey",
            "events_machine_id_fkey",
        ),
        (
            "ALTER TABLE events DROP CONSTRAINT events_app_id_fkey",
            "events_app_id_fkey",
        ),
        (
            "ALTER TABLE events DROP CONSTRAINT events_machine_seq_uidx",
            "events_machine_seq_uidx",
        ),
        (
            "ALTER TABLE events DROP CONSTRAINT events_seq_positive",
            "events_seq_positive",
        ),
        (
            "ALTER TABLE events DROP CONSTRAINT events_window_order",
            "events_window_order",
        ),
        ("DROP INDEX machines_slug_uidx", "machines_slug_uidx"),
        ("DROP INDEX apps_machine_id_idx", "apps_machine_id_idx"),
        ("DROP INDEX events_started_at_idx", "events_started_at_idx"),
        ("DROP INDEX events_app_id_idx", "events_app_id_idx"),
    ] {
        assert_preflight_rejects_single_mutation(mutation, invariant).await?;
    }
    Ok(())
}

#[tokio::test]
async fn repeated_app_sightings_preserve_first_seen_and_advance_last_seen() -> Result<()> {
    // `upsert_app`'s ON CONFLICT clause deliberately refreshes only app_title
    // and last_seen_at. Nothing read either timestamp back, so adding
    // `first_seen_at = EXCLUDED.first_seen_at` - which destroys the whole
    // meaning of "first seen" - passed the entire suite.
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let first = event(0, "notepad.exe", "Notepad", "notes", "first text", None);
        writer.write_start(&first, SplitReason::Initial).await?;

        let original: (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
            sqlx::query_as("SELECT first_seen_at, last_seen_at FROM apps WHERE app_key = $1")
                .bind("notepad.exe")
                .fetch_one(&db.pool)
                .await?;

        // A later sighting of the same app, under a new display title.
        let later = event(
            5,
            "notepad.exe",
            "Notepad Renamed",
            "notes",
            "later text",
            None,
        );
        writer
            .write_start(&later, SplitReason::WindowTitleChange)
            .await?;

        let refreshed: (chrono::DateTime<Utc>, chrono::DateTime<Utc>, String) = sqlx::query_as(
            "SELECT first_seen_at, last_seen_at, app_title FROM apps WHERE app_key = $1",
        )
        .bind("notepad.exe")
        .fetch_one(&db.pool)
        .await?;

        ensure!(
            refreshed.0 == original.0,
            "first_seen_at moved on a repeat sighting: {:?} -> {:?}",
            original.0,
            refreshed.0
        );
        // Strict, and pinned to the exact expected instant.
        //
        // `>=` was vacuous: the fixtures are event(0, ..) then event(5, ..), so
        // a correct upsert must move last_seen_at from at(0) to at(5). Deleting
        // `last_seen_at = EXCLUDED.last_seen_at` from the ON CONFLICT clause -
        // nothing else ever writes the column, since the DEFAULT now() fires
        // only on INSERT - leaves it frozen at at(0), and at(0) >= at(0) passed
        // green. The column would have been stuck at first sighting for the
        // life of the database.
        ensure!(
            refreshed.1 == at(5),
            "last_seen_at did not advance to the repeat sighting's timestamp: {:?} (expected {:?})",
            refreshed.1,
            at(5)
        );
        ensure!(refreshed.2 == "Notepad Renamed", "app_title must refresh");
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn merge_never_reaches_an_event_owned_by_another_machine() -> Result<()> {
    // `write_merge` scopes its UPDATE with `AND machine_id = $10`. Every other
    // test uses a single machine, so that clause could be dropped and one
    // machine's writer would happily overwrite another machine's event row -
    // silently corrupting rows it does not own. Two machines share the
    // disposable schema here, exactly as they would share a real database.
    let db = TestDatabase::create().await?;
    let test_result = async {
        let icarus = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let daedalus =
            PgEventWriter::connect(&db.scoped_url, "daedalus", "Daedalus-Desktop").await?;

        let owned = event(0, "notepad.exe", "Notepad", "owned", "owned text", None);
        let icarus_id = icarus.write_start(&owned, SplitReason::Initial).await?;
        ensure!(icarus_id == "icarus_1");

        let mut intruder = event(5, "msedge.exe", "Edge", "stolen", "stolen text", None);
        intruder.sample_count = 2;

        let result = daedalus.write_merge(&icarus_id, &intruder).await;
        ensure!(
            result.is_err(),
            "a writer merged into an event belonging to another machine"
        );

        // Failing is not enough - prove the row is untouched.
        let row =
            sqlx::query("SELECT window_title, ocr_text, sample_count FROM events WHERE id = $1")
                .bind(&icarus_id)
                .fetch_one(&db.pool)
                .await?;
        ensure!(row.try_get::<String, _>("window_title")? == "owned");
        ensure!(row.try_get::<String, _>("ocr_text")? == "owned text");
        ensure!(row.try_get::<i32, _>("sample_count")? == 1);
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn preflight_verifies_postgres_schema_and_machine_identity() -> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let report = writer.preflight().await?;

        ensure!(report.server_version_num >= 180_000);
        ensure!(report.machine_slug == "icarus");
        ensure!(report.display_name == "Icarus-Laptop");
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn preflight_rejects_named_tables_without_writer_columns() -> Result<()> {
    let db = TestDatabase::create_with_schema(MALFORMED_WRITER_SCHEMA).await?;
    let test_result = async {
        let accepted = match PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await
        {
            Ok(writer) => writer.preflight().await.is_ok(),
            Err(_) => false,
        };
        let machine_count: i64 = sqlx::query_scalar("SELECT count(*) FROM machines")
            .fetch_one(&db.pool)
            .await?;

        ensure!(
            !accepted && machine_count == 0,
            "preflight accepted malformed tables or wrote machine identity before rejection"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn preflight_rejects_missing_app_conflict_constraint() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE apps DROP CONSTRAINT apps_machine_app_key_uidx",
        "apps machine/app-key uniqueness",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_missing_event_machine_started_index() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "DROP INDEX events_machine_started_idx",
        "events machine/started index",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_non_generated_search_column() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE events ALTER COLUMN search_tsv DROP EXPRESSION",
        "generated search column",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_missing_search_gin_index() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "DROP INDEX events_search_tsv_gin",
        "events search GIN index",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_writer_breaking_extra_required_event_column() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE events ADD COLUMN blocker TEXT NOT NULL",
        "exact authoritative event column set",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_space_text_default_literal() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE events ALTER COLUMN window_title SET DEFAULT ' '",
        "space text default differs from empty text default",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_each_authoritative_default_drift() -> Result<()> {
    for (invariant, mutation) in [
        (
            "machines id default",
            "ALTER TABLE machines ALTER COLUMN id SET DEFAULT 1",
        ),
        (
            "machines sequence default",
            "ALTER TABLE machines ALTER COLUMN next_event_seq SET DEFAULT 2",
        ),
        (
            "machines creation default",
            "ALTER TABLE machines ALTER COLUMN created_at SET DEFAULT clock_timestamp()",
        ),
        (
            "apps id default",
            "ALTER TABLE apps ALTER COLUMN id SET DEFAULT 1",
        ),
        (
            "apps title default",
            "ALTER TABLE apps ALTER COLUMN app_title SET DEFAULT 'wrong'",
        ),
        (
            "apps first-seen default",
            "ALTER TABLE apps ALTER COLUMN first_seen_at SET DEFAULT clock_timestamp()",
        ),
        (
            "apps last-seen default",
            "ALTER TABLE apps ALTER COLUMN last_seen_at SET DEFAULT clock_timestamp()",
        ),
        (
            "events kind default",
            "ALTER TABLE events ALTER COLUMN kind SET DEFAULT 'other'",
        ),
        (
            "events ingestion default",
            "ALTER TABLE events ALTER COLUMN ingested_at SET DEFAULT clock_timestamp()",
        ),
        (
            "events window-text default",
            "ALTER TABLE events ALTER COLUMN window_title SET DEFAULT 'wrong'",
        ),
        (
            "events OCR-text default",
            "ALTER TABLE events ALTER COLUMN ocr_text SET DEFAULT 'wrong'",
        ),
        (
            "events readable-text default",
            "ALTER TABLE events ALTER COLUMN readable_text SET DEFAULT 'wrong'",
        ),
        (
            "events caption default absence",
            "ALTER TABLE events ALTER COLUMN caption SET DEFAULT 'wrong'",
        ),
        (
            "events title default absence",
            "ALTER TABLE events ALTER COLUMN title SET DEFAULT 'wrong'",
        ),
        (
            "events hash default",
            "ALTER TABLE events ALTER COLUMN ocr_text_hash SET DEFAULT 'wrong'",
        ),
        (
            "events sample-count default",
            "ALTER TABLE events ALTER COLUMN sample_count SET DEFAULT 2",
        ),
        (
            "events merge-meta default",
            "ALTER TABLE events ALTER COLUMN merge_meta SET DEFAULT '{\"wrong\":true}'::jsonb",
        ),
        (
            "events creation default",
            "ALTER TABLE events ALTER COLUMN created_at SET DEFAULT clock_timestamp()",
        ),
        (
            "events update default",
            "ALTER TABLE events ALTER COLUMN updated_at SET DEFAULT clock_timestamp()",
        ),
    ] {
        assert_preflight_rejects_single_mutation(mutation, invariant)
            .await
            .with_context(|| format!("verify {invariant}"))?;
    }
    Ok(())
}

#[tokio::test]
async fn preflight_rejects_weakened_check_constraint_definition() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE events DROP CONSTRAINT events_sample_count_positive; \
         ALTER TABLE events ADD CONSTRAINT events_sample_count_positive \
         CHECK (sample_count >= 1 OR true)",
        "exact sample-count check definition",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_space_check_constraint_literal() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "ALTER TABLE events DROP CONSTRAINT events_kind_nonempty; \
         ALTER TABLE events ADD CONSTRAINT events_kind_nonempty CHECK (kind <> ' ')",
        "space CHECK literal differs from empty CHECK literal",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_weakened_partial_index_predicate() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "DROP INDEX events_ocr_text_hash_idx; \
         CREATE INDEX events_ocr_text_hash_idx ON events (ocr_text_hash) \
         WHERE ocr_text_hash <> '' OR true",
        "exact OCR hash partial-index predicate",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_space_partial_index_predicate_literal() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        "DROP INDEX events_ocr_text_hash_idx; \
         CREATE INDEX events_ocr_text_hash_idx ON events (ocr_text_hash) \
         WHERE ocr_text_hash <> ' '",
        "space partial-index literal differs from empty partial-index literal",
    )
    .await
}

#[tokio::test]
async fn preflight_rejects_swapped_readable_and_ocr_search_weights() -> Result<()> {
    assert_preflight_rejects_single_mutation(
        r#"
        DROP INDEX events_search_tsv_gin;
        ALTER TABLE events DROP COLUMN search_tsv;
        ALTER TABLE events ADD COLUMN search_tsv TSVECTOR GENERATED ALWAYS AS (
            setweight(to_tsvector('english', coalesce(title, '')), 'A')
            || setweight(to_tsvector('english', coalesce(caption, '')), 'A')
            || setweight(to_tsvector('english', coalesce(readable_text, '')), 'C')
            || setweight(to_tsvector('english', coalesce(ocr_text, '')), 'B')
            || setweight(to_tsvector('english', coalesce(window_title, '')), 'D')
        ) STORED;
        CREATE INDEX events_search_tsv_gin ON events USING GIN (search_tsv);
        "#,
        "ordered generated FTS field weights",
    )
    .await
}

#[tokio::test]
async fn explicit_postgres_writer_creates_no_sqlite_or_file_backend() -> Result<()> {
    let db = TestDatabase::create().await?;
    let temp = tempfile::tempdir()?;
    let sqlite_path = temp.path().join("screen-memory.sqlite");
    unsafe {
        env::set_var(
            "DATABASE_URL",
            format!("sqlite://{}", sqlite_path.display()),
        )
    };
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
    writer
        .write_start(
            &event(0, "notepad.exe", "Notepad", "notes", "text", None),
            SplitReason::Initial,
        )
        .await?;
    unsafe { env::remove_var("DATABASE_URL") };
    ensure!(!sqlite_path.exists());
    db.cleanup().await
}

#[tokio::test]
#[ignore = "post-suite audit for disposable PostgreSQL schema cleanup"]
async fn no_disposable_writer_schemas_remain() -> Result<()> {
    let database_url = env::var("SCREEN_MEMORY_DATABASE_URL")
        .context("SCREEN_MEMORY_DATABASE_URL must be injected for PostgreSQL tests")?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .context("connect disposable-schema auditor")?;
    let schema_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM information_schema.schemata \
         WHERE schema_name LIKE 'goal1_writer\\_%' ESCAPE '\\'",
    )
    .fetch_one(&pool)
    .await
    .context("count disposable writer schemas")?;
    pool.close().await;
    println!("goal1_writer_schema_count={schema_count}");
    ensure!(schema_count == 0, "disposable writer schemas remain");
    Ok(())
}

#[tokio::test]
async fn search_finds_by_words_by_time_and_by_both() -> Result<()> {
    // The read path. Until it existed this system was write-only: it recorded
    // continuously and offered no way to ask it anything, which made every
    // other property of it unverifiable by a person.
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;

        // Two events an hour apart, with distinct text.
        let mut old = event(0, "code.exe", "Code", "editor", "merge contract", None);
        old.latest.readable_text = "reviewing the deterministic merge contract".to_owned();
        old.started_at = at(0);
        old.ended_at = at(10);
        let old_id = writer.write_start(&old, SplitReason::Initial).await?;

        let mut recent = event(20, "chrome.exe", "Chrome", "browser", "release notes", None);
        recent.latest.readable_text = "reading the postgres release notes".to_owned();
        recent.started_at = at(50);
        recent.ended_at = at(59);
        let recent_id = writer.write_start(&recent, SplitReason::AppChange).await?;

        // 1. Words alone.
        let by_word = writer
            .search(&screenpipe_memory::SearchRequest {
                query: "postgres".to_owned(),
                limit: 10,
                ..Default::default()
            })
            .await?;
        ensure!(
            by_word
                .iter()
                .map(|h| h.event_id.as_str())
                .eq([recent_id.as_str()]),
            "keyword search returned {:?}",
            by_word.iter().map(|h| &h.event_id).collect::<Vec<_>>()
        );
        ensure!(
            by_word[0].snippet.contains('['),
            "a keyword hit must bracket the match: {:?}",
            by_word[0].snippet
        );

        // 2. Time alone - the "what was I doing then" question, which has no
        //    keywords in it at all.
        let by_time = writer
            .search(&screenpipe_memory::SearchRequest {
                query: String::new(),
                limit: 10,
                since: Some(at(40)),
                ..Default::default()
            })
            .await?;
        ensure!(
            by_time
                .iter()
                .map(|h| h.event_id.as_str())
                .eq([recent_id.as_str()]),
            "time-only browse returned {:?}",
            by_time.iter().map(|h| &h.event_id).collect::<Vec<_>>()
        );

        // 3. The window must actually EXCLUDE. A filter that is accepted and
        //    ignored is worse than none, because it looks like an answer.
        let excluded = writer
            .search(&screenpipe_memory::SearchRequest {
                query: "postgres".to_owned(),
                limit: 10,
                until: Some(at(30)),
                ..Default::default()
            })
            .await?;
        ensure!(
            excluded.is_empty(),
            "the until bound did not exclude a later event: {:?}",
            excluded.iter().map(|h| &h.event_id).collect::<Vec<_>>()
        );

        // 4. Both together, selecting the older event.
        let both = writer
            .search(&screenpipe_memory::SearchRequest {
                query: "merge".to_owned(),
                limit: 10,
                until: Some(at(30)),
                ..Default::default()
            })
            .await?;
        ensure!(
            both.iter()
                .map(|h| h.event_id.as_str())
                .eq([old_id.as_str()]),
            "combined search returned {:?}",
            both.iter().map(|h| &h.event_id).collect::<Vec<_>>()
        );

        // 5. A request with neither words nor a window is not a question.
        ensure!(
            writer
                .search(&screenpipe_memory::SearchRequest {
                    query: "   ".to_owned(),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .is_err(),
            "a blank query with no time window must be refused, not answered with everything"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn search_headline_includes_context_for_a_match_after_twenty_thousand_characters()
-> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let mut late = event(1, "notepad.exe", "Notepad", "notes", "early OCR", None);
        late.latest.readable_text = format!("{}lateheadlinefixturemarker", "filler ".repeat(3_500));
        let event_id = writer.write_start(&late, SplitReason::Initial).await?;

        let hits = writer
            .search(&screenpipe_memory::SearchRequest {
                query: "lateheadlinefixturemarker".to_owned(),
                limit: 10,
                ..Default::default()
            })
            .await?;

        ensure!(
            hits.iter()
                .map(|hit| hit.event_id.as_str())
                .eq([event_id.as_str()]),
            "the complete FTS corpus must find the late marker: {hits:?}"
        );
        ensure!(
            hits[0].snippet.contains("[lateheadlinefixturemarker]"),
            "the headline must show context around its late match: {:?}",
            hits[0].snippet
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn explicit_machine_search_connection_never_upserts_a_machine() -> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        writer
            .write_start(
                &event(1, "notepad.exe", "Notepad", "notes", "searchable", None),
                SplitReason::Initial,
            )
            .await?;
        let before: (i64, String, i64) = sqlx::query_as(
            "SELECT id, display_name, next_event_seq FROM machines WHERE slug = $1",
        )
        .bind("icarus")
        .fetch_one(&db.pool)
        .await?;

        let reader = PgEventReader::connect(&db.scoped_url, "icarus").await?;
        let hits = reader
            .search(&screenpipe_memory::SearchRequest {
                query: "searchable".to_owned(),
                limit: 10,
                ..Default::default()
            })
            .await?;
        let after: (i64, String, i64) = sqlx::query_as(
            "SELECT id, display_name, next_event_seq FROM machines WHERE slug = $1",
        )
        .bind("icarus")
        .fetch_one(&db.pool)
        .await?;

        ensure!(
            hits.len() == 1,
            "the explicit machine reader did not return its recorded event"
        );
        ensure!(
            before == after,
            "search must not upsert or otherwise mutate the explicit machine: before={before:?} after={after:?}"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn writer_backfills_missing_titles_once_for_existing_events() -> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let event_id = writer
            .write_start(
                &event(
                    1,
                    "notepad.exe",
                    "  Notepad  Editor  ",
                    "  project notes  ",
                    "text",
                    None,
                ),
                SplitReason::Initial,
            )
            .await?;
        sqlx::query("UPDATE events SET title = NULL WHERE id = $1")
            .bind(&event_id)
            .execute(&db.pool)
            .await?;

        let _ = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let first: (Option<String>, String) =
            sqlx::query_as("SELECT title, xmin::text FROM events WHERE id = $1")
                .bind(&event_id)
                .fetch_one(&db.pool)
                .await?;
        let _ = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let second: (Option<String>, String) =
            sqlx::query_as("SELECT title, xmin::text FROM events WHERE id = $1")
                .bind(&event_id)
                .fetch_one(&db.pool)
                .await?;

        ensure!(
            first.0 == Some("Notepad Editor - project notes".to_owned()),
            "the title migration did not backfill the event: {first:?}"
        );
        ensure!(
            first == second,
            "a completed title migration must not rewrite rows: first={first:?} second={second:?}"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}

#[tokio::test]
async fn writer_backfill_leaves_blank_derived_titles_null_without_rewriting_them() -> Result<()> {
    let db = TestDatabase::create().await?;
    let test_result = async {
        let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let event_id = writer
            .write_start(
                &event(
                    1,
                    "notepad.exe",
                    " \t\r\n ",
                    "\n\t  ",
                    "text",
                    None,
                ),
                SplitReason::Initial,
            )
            .await?;
        sqlx::query("UPDATE events SET title = NULL WHERE id = $1")
            .bind(&event_id)
            .execute(&db.pool)
            .await?;
        let before: (Option<String>, String) =
            sqlx::query_as("SELECT title, xmin::text FROM events WHERE id = $1")
                .bind(&event_id)
                .fetch_one(&db.pool)
                .await?;

        let _ = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let first: (Option<String>, String) =
            sqlx::query_as("SELECT title, xmin::text FROM events WHERE id = $1")
                .bind(&event_id)
                .fetch_one(&db.pool)
                .await?;
        let _ = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
        let second: (Option<String>, String) =
            sqlx::query_as("SELECT title, xmin::text FROM events WHERE id = $1")
                .bind(&event_id)
                .fetch_one(&db.pool)
                .await?;

        ensure!(before.0.is_none(), "the blank title fixture must begin NULL: {before:?}");
        ensure!(first.0.is_none(), "the first writer connection filled a blank title: {first:?}");
        ensure!(second.0.is_none(), "the second writer connection filled a blank title: {second:?}");
        ensure!(
            before == first && first == second,
            "blank derived titles must not create row versions: before={before:?} first={first:?} second={second:?}"
        );
        Ok(())
    }
    .await;
    db.finish(test_result).await
}
