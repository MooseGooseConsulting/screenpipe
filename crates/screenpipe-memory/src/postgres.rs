use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};

use crate::{EventId, EventSink, OpenEvent, SplitReason};

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

        let schema_present = sqlx::query_scalar::<_, bool>(
            "SELECT count(DISTINCT table_name) = 3 \
             FROM information_schema.tables \
             WHERE table_schema = current_schema() \
               AND table_name IN ('machines', 'apps', 'events')",
        )
        .fetch_one(&self.pool)
        .await
        .context("inspect authoritative PostgreSQL schema")?;
        ensure!(
            schema_present,
            "authoritative PostgreSQL schema is incomplete"
        );

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
