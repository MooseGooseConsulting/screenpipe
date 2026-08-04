use std::collections::BTreeMap;
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGapSummary, MERGE_CONTRACT_VERSION, MergeDecisionKind,
    ObservationSample, OpenEvent, PgEventWriter, SplitReason,
};
use serde_json::Value;
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

struct TestDatabase {
    admin_pool: PgPool,
    pool: PgPool,
    scoped_url: String,
    schema: String,
}

impl TestDatabase {
    async fn create() -> Result<Self> {
        let database_url = env::var("SCREEN_MEMORY_DATABASE_URL")
            .context("SCREEN_MEMORY_DATABASE_URL must be injected for PostgreSQL tests")?;
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
        sqlx::raw_sql(AUTHORITATIVE_SCHEMA)
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
        },
        sample_count: 1,
        hash_counts: BTreeMap::from([(hash, 1)]),
    }
}

#[tokio::test]
async fn starts_allocate_icarus_ids_and_persist_authoritative_event_fields() -> Result<()> {
    let db = TestDatabase::create().await?;
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;
    let first = event(0, "notepad.exe", "Notepad", "notes", "first text", None);
    let second = event(
        1,
        "msedge.exe",
        "Microsoft Edge",
        "browser",
        "second text",
        Some("https://example.test/path"),
    );

    let first_id = writer.write_start(&first, SplitReason::Initial).await?;
    let second_id = writer.write_start(&second, SplitReason::AppChange).await?;
    let row = sqlx::query(
        "SELECT e.id, e.seq, e.kind, e.window_title, e.ocr_text, e.readable_text, \
                e.ocr_text_hash, e.sample_count, e.merge_meta, m.slug, m.display_name, \
                a.app_key, a.app_title \
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
    ensure!(row.try_get::<String, _>("window_title")? == "browser");
    ensure!(row.try_get::<String, _>("ocr_text")? == "second text");
    ensure!(row.try_get::<String, _>("readable_text")? == "readable second text");
    ensure!(row.try_get::<String, _>("ocr_text_hash")? == "hash-1");
    ensure!(row.try_get::<i32, _>("sample_count")? == 1);
    ensure!(row.try_get::<String, _>("slug")? == "icarus");
    ensure!(row.try_get::<String, _>("display_name")? == "Icarus-Laptop");
    ensure!(row.try_get::<String, _>("app_key")? == "msedge.exe");
    ensure!(row.try_get::<String, _>("app_title")? == "Microsoft Edge");
    ensure!(meta["start_reason"] == "app_change");
    ensure!(meta["browser_url"] == "https://example.test/path");
    ensure!(meta["hashes_seen"]["hash-1"] == 1);
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
    merged.hash_counts = BTreeMap::from([("hash-0".to_owned(), 1), ("hash-5".to_owned(), 1)]);

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
    ensure!(meta["last_decision"] == "merge");
    ensure!(meta["browser_url"] == "https://example.test/latest");
    ensure!(meta["hashes_seen"]["hash-0"] == 1);
    ensure!(meta["hashes_seen"]["hash-5"] == 1);
    ensure!(meta["capture_gaps"]["empty_ocr"] == 3);
    db.cleanup().await
}

#[tokio::test]
async fn preflight_verifies_postgres_schema_and_machine_identity() -> Result<()> {
    let db = TestDatabase::create().await?;
    let writer = PgEventWriter::connect(&db.scoped_url, "icarus", "Icarus-Laptop").await?;

    let report = writer.preflight().await?;

    ensure!(report.server_version_num >= 180_000);
    ensure!(report.schema_present);
    ensure!(report.machine_slug == "icarus");
    ensure!(report.display_name == "Icarus-Laptop");
    db.cleanup().await
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
