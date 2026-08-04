use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};

use crate::{EventId, EventSink, OpenEvent, SplitReason};

const REQUIRED_COLUMNS_SQL: &str = r#"
WITH expected(table_name, column_name, type_name, not_null, generated) AS (
    VALUES
        ('machines', 'id', 'bigint', true, ''),
        ('machines', 'slug', 'text', true, ''),
        ('machines', 'display_name', 'text', true, ''),
        ('machines', 'next_event_seq', 'bigint', true, ''),
        ('machines', 'created_at', 'timestamp with time zone', true, ''),
        ('apps', 'id', 'bigint', true, ''),
        ('apps', 'machine_id', 'bigint', true, ''),
        ('apps', 'app_key', 'text', true, ''),
        ('apps', 'app_title', 'text', true, ''),
        ('apps', 'first_seen_at', 'timestamp with time zone', true, ''),
        ('apps', 'last_seen_at', 'timestamp with time zone', true, ''),
        ('events', 'id', 'text', true, ''),
        ('events', 'machine_id', 'bigint', true, ''),
        ('events', 'seq', 'bigint', true, ''),
        ('events', 'kind', 'text', true, ''),
        ('events', 'started_at', 'timestamp with time zone', true, ''),
        ('events', 'ended_at', 'timestamp with time zone', true, ''),
        ('events', 'ingested_at', 'timestamp with time zone', true, ''),
        ('events', 'app_id', 'bigint', false, ''),
        ('events', 'window_title', 'text', true, ''),
        ('events', 'ocr_text', 'text', true, ''),
        ('events', 'readable_text', 'text', true, ''),
        ('events', 'caption', 'text', false, ''),
        ('events', 'title', 'text', false, ''),
        ('events', 'ocr_text_hash', 'text', true, ''),
        ('events', 'sample_count', 'integer', true, ''),
        ('events', 'merge_meta', 'jsonb', true, ''),
        ('events', 'created_at', 'timestamp with time zone', true, ''),
        ('events', 'updated_at', 'timestamp with time zone', true, ''),
        ('events', 'search_tsv', 'tsvector', false, 's')
),
actual AS MATERIALIZED (
    SELECT c.relname::text AS table_name,
           a.attname::text AS column_name,
           format_type(a.atttypid, a.atttypmod) AS type_name,
           a.attnotnull AS not_null,
           a.attgenerated::text AS generated
    FROM pg_catalog.pg_attribute a
    JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema()
      AND c.relkind = 'r'
      AND a.attnum > 0
      AND NOT a.attisdropped
)
SELECT count(*) = (SELECT count(*) FROM expected)
   AND (SELECT count(*) FROM actual) = (SELECT count(*) FROM expected)
FROM expected e
JOIN actual a USING (table_name, column_name)
WHERE a.type_name = e.type_name
  AND a.not_null = e.not_null
  AND a.generated = e.generated
"#;

const REQUIRED_DEFAULTS_SQL: &str = r#"
WITH expected(table_name, column_name, expression) AS (
    VALUES
        ('machines', 'id', 'nextval(''machines_id_seq''::regclass)'),
        ('machines', 'next_event_seq', '1'),
        ('machines', 'created_at', 'now()'),
        ('apps', 'id', 'nextval(''apps_id_seq''::regclass)'),
        ('apps', 'app_title', '''''::text'),
        ('apps', 'first_seen_at', 'now()'),
        ('apps', 'last_seen_at', 'now()'),
        ('events', 'kind', '''screen''::text'),
        ('events', 'ingested_at', 'now()'),
        ('events', 'window_title', '''''::text'),
        ('events', 'ocr_text', '''''::text'),
        ('events', 'readable_text', '''''::text'),
        ('events', 'ocr_text_hash', '''''::text'),
        ('events', 'sample_count', '1'),
        ('events', 'merge_meta', '''{}''::jsonb'),
        ('events', 'created_at', 'now()'),
        ('events', 'updated_at', 'now()')
),
actual AS MATERIALIZED (
    SELECT c.relname::text AS table_name,
           a.attname::text AS column_name,
           pg_get_expr(d.adbin, d.adrelid) AS expression
    FROM pg_catalog.pg_attrdef d
    JOIN pg_catalog.pg_attribute a
      ON a.attrelid = d.adrelid AND a.attnum = d.adnum
    JOIN pg_catalog.pg_class c ON c.oid = d.adrelid
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema()
      AND c.relname IN ('machines', 'apps', 'events')
      AND a.attgenerated = ''
)
SELECT count(*) = (SELECT count(*) FROM expected)
   AND (SELECT count(*) FROM actual) = (SELECT count(*) FROM expected)
FROM expected e
JOIN actual a USING (table_name, column_name)
WHERE a.expression = e.expression
"#;

const REQUIRED_CONSTRAINTS_SQL: &str = r#"
WITH expected(constraint_name, table_name, constraint_type, columns, referenced_table, definition) AS (
    VALUES
        ('machines_pkey', 'machines', 'p', ARRAY['id']::text[], '', 'PRIMARY KEY (id)'),
        ('machines_slug_format', 'machines', 'c', ARRAY['slug']::text[], '', 'CHECK ((slug ~ ''^[a-z][a-z0-9_]*$''::text))'),
        ('machines_next_event_seq_positive', 'machines', 'c', ARRAY['next_event_seq']::text[], '', 'CHECK ((next_event_seq >= 1))'),
        ('apps_pkey', 'apps', 'p', ARRAY['id']::text[], '', 'PRIMARY KEY (id)'),
        ('apps_machine_id_fkey', 'apps', 'f', ARRAY['machine_id']::text[], 'machines', 'FOREIGN KEY (machine_id) REFERENCES machines(id)'),
        ('apps_machine_app_key_uidx', 'apps', 'u', ARRAY['machine_id', 'app_key']::text[], '', 'UNIQUE (machine_id, app_key)'),
        ('events_pkey', 'events', 'p', ARRAY['id']::text[], '', 'PRIMARY KEY (id)'),
        ('events_machine_id_fkey', 'events', 'f', ARRAY['machine_id']::text[], 'machines', 'FOREIGN KEY (machine_id) REFERENCES machines(id)'),
        ('events_app_id_fkey', 'events', 'f', ARRAY['app_id']::text[], 'apps', 'FOREIGN KEY (app_id) REFERENCES apps(id)'),
        ('events_machine_seq_uidx', 'events', 'u', ARRAY['machine_id', 'seq']::text[], '', 'UNIQUE (machine_id, seq)'),
        ('events_seq_positive', 'events', 'c', ARRAY['seq']::text[], '', 'CHECK ((seq >= 1))'),
        ('events_sample_count_positive', 'events', 'c', ARRAY['sample_count']::text[], '', 'CHECK ((sample_count >= 1))'),
        ('events_window_order', 'events', 'c', ARRAY['started_at', 'ended_at']::text[], '', 'CHECK ((ended_at >= started_at))'),
        ('events_kind_nonempty', 'events', 'c', ARRAY['kind']::text[], '', 'CHECK ((kind <> ''''::text))')
),
actual AS MATERIALIZED (
    SELECT con.conname::text AS constraint_name,
           table_class.relname::text AS table_name,
           con.contype::text AS constraint_type,
           ARRAY(
               SELECT attribute.attname::text
               FROM unnest(con.conkey) AS key(attnum)
               JOIN pg_catalog.pg_attribute attribute
                 ON attribute.attrelid = con.conrelid
                AND attribute.attnum = key.attnum
               ORDER BY attribute.attname
           ) AS columns,
           coalesce(referenced_class.relname::text, '') AS referenced_table,
           pg_get_constraintdef(con.oid) AS definition,
           con.convalidated AS validated
    FROM pg_catalog.pg_constraint con
    JOIN pg_catalog.pg_class table_class ON table_class.oid = con.conrelid
    JOIN pg_catalog.pg_namespace n ON n.oid = table_class.relnamespace
    LEFT JOIN pg_catalog.pg_class referenced_class ON referenced_class.oid = con.confrelid
    WHERE n.nspname = current_schema()
)
SELECT count(*) = (SELECT count(*) FROM expected)
FROM expected e
JOIN actual a USING (constraint_name, table_name, constraint_type, referenced_table)
WHERE a.validated
  AND a.columns @> e.columns
  AND e.columns @> a.columns
  AND a.definition = e.definition
"#;

const REQUIRED_INDEXES_SQL: &str = r#"
WITH expected(index_name, table_name, method_name, unique_index, columns, sort_options, predicate) AS (
    VALUES
        ('machines_slug_uidx', 'machines', 'btree', true, ARRAY['slug']::text[], '0', ''),
        ('apps_machine_id_idx', 'apps', 'btree', false, ARRAY['machine_id']::text[], '0', ''),
        ('events_machine_started_idx', 'events', 'btree', false, ARRAY['machine_id', 'started_at']::text[], '0 3', ''),
        ('events_started_at_idx', 'events', 'btree', false, ARRAY['started_at']::text[], '3', ''),
        ('events_ocr_text_hash_idx', 'events', 'btree', false, ARRAY['ocr_text_hash']::text[], '0', '(ocr_text_hash <> ''''::text)'),
        ('events_app_id_idx', 'events', 'btree', false, ARRAY['app_id']::text[], '0', '(app_id IS NOT NULL)'),
        ('events_search_tsv_gin', 'events', 'gin', false, ARRAY['search_tsv']::text[], '0', '')
),
actual AS MATERIALIZED (
    SELECT index_class.relname::text AS index_name,
           table_class.relname::text AS table_name,
           access_method.amname::text AS method_name,
           index.indisunique AS unique_index,
           ARRAY(
               SELECT attribute.attname::text
               FROM unnest(index.indkey) WITH ORDINALITY AS key(attnum, position)
               JOIN pg_catalog.pg_attribute attribute
                 ON attribute.attrelid = index.indrelid
                AND attribute.attnum = key.attnum
               WHERE key.position <= index.indnkeyatts
               ORDER BY key.position
           ) AS columns,
           index.indoption::text AS sort_options,
           coalesce(pg_get_expr(index.indpred, index.indrelid), '') AS predicate,
           index.indisvalid AS valid,
           index.indisready AS ready
    FROM pg_catalog.pg_index index
    JOIN pg_catalog.pg_class index_class ON index_class.oid = index.indexrelid
    JOIN pg_catalog.pg_class table_class ON table_class.oid = index.indrelid
    JOIN pg_catalog.pg_namespace n ON n.oid = table_class.relnamespace
    JOIN pg_catalog.pg_am access_method ON access_method.oid = index_class.relam
    WHERE n.nspname = current_schema()
)
SELECT count(*) = (SELECT count(*) FROM expected)
FROM expected e
JOIN actual a USING (index_name, table_name, method_name, unique_index, columns, sort_options, predicate)
WHERE a.valid AND a.ready
"#;

const GENERATED_SEARCH_SQL: &str = r#"
WITH expected(expression) AS (
    VALUES ($fts$((((setweight(to_tsvector('english'::regconfig, COALESCE(title, ''::text)), 'A'::"char") || setweight(to_tsvector('english'::regconfig, COALESCE(caption, ''::text)), 'A'::"char")) || setweight(to_tsvector('english'::regconfig, COALESCE(readable_text, ''::text)), 'B'::"char")) || setweight(to_tsvector('english'::regconfig, COALESCE(ocr_text, ''::text)), 'C'::"char")) || setweight(to_tsvector('english'::regconfig, COALESCE(window_title, ''::text)), 'D'::"char"))$fts$)
),
target AS MATERIALIZED (
    SELECT a.attgenerated,
           pg_get_expr(d.adbin, d.adrelid) AS expression
    FROM pg_catalog.pg_attribute a
    JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE n.nspname = current_schema()
      AND c.relname = 'events'
      AND a.attname = 'search_tsv'
)
SELECT count(*) = 1
FROM target
JOIN expected USING (expression)
WHERE target.attgenerated = 's'
"#;

pub struct PgEventWriter {
    pool: PgPool,
    machine_id: i64,
    machine_slug: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgPreflight {
    pub server_version: String,
    pub server_version_num: i32,
    pub schema_present: bool,
    pub machine_slug: String,
    pub display_name: String,
}

impl PgEventWriter {
    pub async fn connect(database_url: &str, slug: &str, display_name: &str) -> Result<Self> {
        if database_url.trim().is_empty() {
            bail!("PostgreSQL database URL is blank");
        }
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .context("connect PostgreSQL event writer")?;
        validate_authoritative_schema(&pool).await?;
        let machine_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO machines (slug, display_name) VALUES ($1, $2) \
             ON CONFLICT (slug) DO UPDATE SET display_name = EXCLUDED.display_name \
             RETURNING id",
        )
        .bind(slug)
        .bind(display_name)
        .fetch_one(&pool)
        .await
        .context("upsert writer machine")?;
        Ok(Self {
            pool,
            machine_id,
            machine_slug: slug.to_owned(),
        })
    }

    pub async fn preflight(&self) -> Result<PgPreflight> {
        validate_authoritative_schema(&self.pool).await?;
        let (server_version, server_version_num) = sqlx::query_as::<_, (String, i32)>(
            "SELECT current_setting('server_version'), \
                    current_setting('server_version_num')::integer",
        )
        .fetch_one(&self.pool)
        .await
        .context("read PostgreSQL server version")?;
        ensure!(
            server_version_num >= 180_000,
            "PostgreSQL 18 or newer is required"
        );

        let schema_present = true;

        let (machine_id, machine_slug, display_name, next_event_seq) =
            sqlx::query_as::<_, (i64, String, String, i64)>(
                "SELECT id, slug, display_name, next_event_seq \
                 FROM machines WHERE slug = $1",
            )
            .bind(&self.machine_slug)
            .fetch_one(&self.pool)
            .await
            .context("verify PostgreSQL machine identity")?;
        ensure!(
            machine_id == self.machine_id,
            "PostgreSQL machine identity changed"
        );
        ensure!(
            next_event_seq >= 1,
            "PostgreSQL machine sequence is invalid"
        );

        Ok(PgPreflight {
            server_version,
            server_version_num,
            schema_present,
            machine_slug,
            display_name,
        })
    }

    pub async fn write_start(&self, event: &OpenEvent, reason: SplitReason) -> Result<String> {
        let sample_count = checked_sample_count(event)?;
        let mut transaction = self.pool.begin().await.context("begin event start")?;
        let (machine_id, slug, sequence) = sqlx::query_as::<_, (i64, String, i64)>(
            "UPDATE machines \
                 SET next_event_seq = next_event_seq + 1 \
                 WHERE slug = $1 \
                 RETURNING id AS machine_id, slug, next_event_seq - 1 AS seq",
        )
        .bind(&self.machine_slug)
        .fetch_one(&mut *transaction)
        .await
        .context("allocate machine event sequence")?;
        ensure!(
            machine_id == self.machine_id,
            "allocated machine identity changed"
        );
        let app_id = upsert_app(&mut transaction, machine_id, event).await?;
        let event_id = format!("{slug}_{sequence}");
        let merge_meta = merge_meta(event, reason);
        sqlx::query(
            "INSERT INTO events (\
                 id, machine_id, seq, kind, started_at, ended_at, app_id, window_title, \
                 ocr_text, readable_text, ocr_text_hash, sample_count, merge_meta\
             ) VALUES ($1, $2, $3, 'screen', $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&event_id)
        .bind(machine_id)
        .bind(sequence)
        .bind(event.started_at)
        .bind(event.ended_at)
        .bind(app_id)
        .bind(&event.latest.window_title)
        .bind(&event.latest.ocr_text)
        .bind(&event.latest.readable_text)
        .bind(&event.latest_exact_ocr_hash)
        .bind(sample_count)
        .bind(merge_meta)
        .execute(&mut *transaction)
        .await
        .context("insert allocated screen event")?;
        transaction.commit().await.context("commit event start")?;
        Ok(event_id)
    }

    pub async fn write_merge(&self, event_id: &str, event: &OpenEvent) -> Result<()> {
        let sample_count = checked_sample_count(event)?;
        let mut transaction = self.pool.begin().await.context("begin event merge")?;
        let app_id = upsert_app(&mut transaction, self.machine_id, event).await?;
        let merge_meta = merge_meta(event, event.start_reason);
        let result = sqlx::query(
            "UPDATE events SET \
                 ended_at = $1, app_id = $2, window_title = $3, ocr_text = $4, \
                 readable_text = $5, ocr_text_hash = $6, sample_count = $7, \
                 merge_meta = $8, updated_at = now() \
             WHERE id = $9 AND machine_id = $10",
        )
        .bind(event.ended_at)
        .bind(app_id)
        .bind(&event.latest.window_title)
        .bind(&event.latest.ocr_text)
        .bind(&event.latest.readable_text)
        .bind(&event.latest_exact_ocr_hash)
        .bind(sample_count)
        .bind(merge_meta)
        .bind(event_id)
        .bind(self.machine_id)
        .execute(&mut *transaction)
        .await
        .context("update merged screen event")?;
        ensure!(
            result.rows_affected() == 1,
            "event merge target was not found"
        );
        transaction.commit().await.context("commit event merge")?;
        Ok(())
    }
}

async fn validate_authoritative_schema(pool: &PgPool) -> Result<()> {
    for (component, statement) in [
        ("columns", REQUIRED_COLUMNS_SQL),
        ("defaults", REQUIRED_DEFAULTS_SQL),
        ("constraints", REQUIRED_CONSTRAINTS_SQL),
        ("indexes", REQUIRED_INDEXES_SQL),
        ("generated search", GENERATED_SEARCH_SQL),
    ] {
        let compatible = sqlx::query_scalar::<_, bool>(statement)
            .fetch_one(pool)
            .await
            .with_context(|| format!("inspect authoritative PostgreSQL {component}"))?;
        ensure!(
            compatible,
            "authoritative PostgreSQL schema is incompatible: {component}"
        );
    }
    Ok(())
}

#[async_trait]
impl EventSink for PgEventWriter {
    async fn start(&self, event: &OpenEvent, reason: SplitReason) -> Result<EventId> {
        EventId::try_from(self.write_start(event, reason).await?)
    }

    async fn merge(&self, event_id: &str, event: &OpenEvent) -> Result<()> {
        self.write_merge(event_id, event).await
    }
}

async fn upsert_app(
    transaction: &mut Transaction<'_, Postgres>,
    machine_id: i64,
    event: &OpenEvent,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO apps (machine_id, app_key, app_title, first_seen_at, last_seen_at) \
         VALUES ($1, $2, $3, $4, $4) \
         ON CONFLICT (machine_id, app_key) DO UPDATE SET \
             app_title = EXCLUDED.app_title, last_seen_at = EXCLUDED.last_seen_at \
         RETURNING id",
    )
    .bind(machine_id)
    .bind(&event.latest.app_key)
    .bind(&event.latest.app_title)
    .bind(event.latest.captured_at)
    .fetch_one(&mut **transaction)
    .await
    .context("upsert event application")
}

fn checked_sample_count(event: &OpenEvent) -> Result<i32> {
    i32::try_from(event.sample_count).context("event sample count exceeds PostgreSQL integer")
}

fn merge_meta(event: &OpenEvent, start_reason: SplitReason) -> Value {
    json!({
        "merge_contract_version": event.merge_contract_version,
        "start_reason": start_reason.as_code(),
        "last_decision": event.last_decision.as_code(),
        "merge_hash": event.merge_hash,
        "latest_exact_ocr_hash": event.latest_exact_ocr_hash,
        "hashes_seen": event.hash_counts,
        "sample_count": event.sample_count,
        "latest_cadence": {
            "input_idle_ms": event.latest_cadence.input.input_idle.num_milliseconds(),
            "frame_stable_for_ms": event.latest_cadence.input.frame_stable_for.num_milliseconds(),
            "foreground_changed": event.latest_cadence.input.foreground_changed,
            "frame_changed": event.latest_cadence.input.frame_changed,
            "next_interval_ms": event.latest_cadence.next_interval.num_milliseconds(),
        },
        "capture_gaps": {
            "capture_unavailable": event.capture_gaps.capture_unavailable,
            "ocr_unavailable": event.capture_gaps.ocr_unavailable,
            "empty_ocr": event.capture_gaps.empty_ocr,
        },
        "browser_url": event.latest.browser_url,
    })
}
