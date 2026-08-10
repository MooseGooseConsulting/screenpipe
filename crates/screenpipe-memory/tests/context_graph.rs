use std::env;

use anyhow::{Context, Result, ensure};
use screenpipe_memory::{
    ContextModality, MemoryPolicyRepository, PgPolicyRepository, PolicyMutationRequest,
    PolicyRepository, ensure_v3_schema,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

const TEST_DATABASE_URL_ENV: &str = "SCREEN_MEMORY_TEST_DATABASE_URL";

#[tokio::test]
async fn policy_defaults_grant_screen_and_browser_but_not_private_modalities() -> Result<()> {
    let repo = MemoryPolicyRepository::new();

    assert_eq!(
        ContextModality::parse("screen"),
        Some(ContextModality::Screen)
    );
    assert!(ContextModality::Screen.default_consent());
    assert!(!ContextModality::Clipboard.default_consent());
    assert_eq!(ContextModality::parse("unknown"), None);

    let screen_policy = repo.get_policy("source-screen-1", "screen").await?;
    assert!(
        screen_policy.consent,
        "screen capture must remain granted unless policy excludes it"
    );
    assert!(screen_policy.permits_capture());

    let browser_policy = repo.get_policy("source-browser-1", "browser").await?;
    assert!(
        browser_policy.consent,
        "browser capture must remain granted unless policy excludes it"
    );

    for modality in ["clipboard", "audio"] {
        let policy = repo.get_policy("source-private-1", modality).await?;
        assert!(
            !policy.consent,
            "{modality} consent must default to false (ungranted)"
        );
        assert_eq!(policy.policy_epoch, 1);
        assert!(!policy.permits_capture());
    }

    Ok(())
}

#[tokio::test]
async fn policy_mutation_advances_the_epoch_and_rejects_a_stale_snapshot() -> Result<()> {
    let repo = MemoryPolicyRepository::new();

    let snap1 = repo
        .mutate_policy(PolicyMutationRequest {
            source_id: "src-1".to_owned(),
            modality: "clipboard".to_owned(),
            expected_epoch: None,
            consent: true,
            excluded: false,
            retention_class: Some("high_retention".to_owned()),
            reason: "user granted permission".to_owned(),
        })
        .await?;
    assert_eq!(snap1.policy_epoch, 2);
    assert!(snap1.consent);

    let snap2 = repo
        .mutate_policy(PolicyMutationRequest {
            source_id: "src-1".to_owned(),
            modality: "clipboard".to_owned(),
            expected_epoch: Some(2),
            consent: false,
            excluded: true,
            retention_class: None,
            reason: "user revoked permission".to_owned(),
        })
        .await?;
    assert_eq!(snap2.policy_epoch, 3);
    assert!(snap2.excluded);
    assert!(
        !snap2.permits_capture(),
        "exclusion must override a granted or stale consent snapshot"
    );

    let stale = repo
        .mutate_policy(PolicyMutationRequest {
            source_id: "src-1".to_owned(),
            modality: "clipboard".to_owned(),
            expected_epoch: Some(2),
            consent: true,
            excluded: false,
            retention_class: None,
            reason: "stale update attempt".to_owned(),
        })
        .await;
    assert!(stale.is_err(), "a stale policy epoch must be rejected");

    let after = repo.get_policy("src-1", "clipboard").await?;
    assert_eq!(after.policy_epoch, 3);
    assert!(!after.consent);
    assert!(after.excluded);

    Ok(())
}

fn test_database_url() -> Result<Option<String>> {
    let Some(database_url) = env::var(TEST_DATABASE_URL_ENV).ok() else {
        eprintln!("SKIPPED context_graph: {TEST_DATABASE_URL_ENV} is not set");
        return Ok(None);
    };

    let database_name = database_url
        .split('?')
        .next()
        .and_then(|url| url.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .context("test database URL has no database name")?;
    ensure!(
        database_name.ends_with("_test"),
        "refusing to mutate a non-test database through {TEST_DATABASE_URL_ENV}"
    );
    Ok(Some(database_url))
}

async fn reset_v3_schema(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        DROP TABLE IF EXISTS retention_metadata CASCADE;
        DROP TABLE IF EXISTS reconstruction_jobs CASCADE;
        DROP TABLE IF EXISTS embeddings CASCADE;
        DROP TABLE IF EXISTS relations CASCADE;
        DROP TABLE IF EXISTS annotations CASCADE;
        DROP TABLE IF EXISTS text_revisions CASCADE;
        DROP TABLE IF EXISTS gaps CASCADE;
        DROP TABLE IF EXISTS observations CASCADE;
        DROP TABLE IF EXISTS actors CASCADE;
        DROP TABLE IF EXISTS projects CASCADE;
        DROP TABLE IF EXISTS sessions CASCADE;
        DROP TABLE IF EXISTS applications CASCADE;
        DROP TABLE IF EXISTS browser_resources CASCADE;
        DROP TABLE IF EXISTS windows CASCADE;
        DROP TABLE IF EXISTS policy_audit_log CASCADE;
        DROP TABLE IF EXISTS source_policies CASCADE;
        DROP TABLE IF EXISTS sources CASCADE;
        "#,
    )
    .execute(pool)
    .await
    .context("clear v3 context schema")?;
    Ok(())
}

async fn ensure_v1_prerequisites(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS machines (
            id BIGSERIAL PRIMARY KEY,
            slug TEXT NOT NULL UNIQUE,
            display_name TEXT NOT NULL,
            next_event_seq BIGINT NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS apps (
            id BIGSERIAL PRIMARY KEY,
            machine_id BIGINT REFERENCES machines (id),
            app_key TEXT NOT NULL DEFAULT '',
            app_name TEXT NOT NULL DEFAULT ''
        );
        "#,
    )
    .execute(pool)
    .await
    .context("create v1 schema prerequisites")?;
    Ok(())
}

#[tokio::test]
async fn context_graph_migrates_all_v3_entities_and_enforces_schema_integrity() -> Result<()> {
    let Some(database_url) = test_database_url()? else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .context("connect disposable context graph database")?;

    reset_v3_schema(&pool).await?;
    ensure_v1_prerequisites(&pool).await?;
    let result = async {
        ensure_v3_schema(&pool).await?;

        for table in [
            "sources",
            "source_policies",
            "policy_audit_log",
            "windows",
            "browser_resources",
            "applications",
            "sessions",
            "projects",
            "actors",
            "observations",
            "gaps",
            "text_revisions",
            "annotations",
            "relations",
            "embeddings",
            "reconstruction_jobs",
            "retention_metadata",
        ] {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass(current_schema() || '.' || $1) IS NOT NULL",
            )
            .bind(table)
            .fetch_one(&pool)
            .await?;
            assert!(exists, "v3 migration did not create {table}");
        }

        let machine_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO machines (slug, display_name) VALUES ('context-test', 'Context Test') \
             ON CONFLICT (slug) DO UPDATE SET display_name = EXCLUDED.display_name RETURNING id",
        )
        .fetch_one(&pool)
        .await?;
        let policy_repository = PgPolicyRepository::for_machine(pool.clone(), machine_id);
        let policy = policy_repository
            .mutate_policy(PolicyMutationRequest {
                source_id: "source-policy-test".to_owned(),
                modality: "clipboard".to_owned(),
                expected_epoch: Some(1),
                consent: true,
                excluded: false,
                retention_class: None,
                reason: "grant a disposable test policy".to_owned(),
            })
            .await?;
        assert_eq!(policy.policy_epoch, 2, "first policy mutation advances epoch one");
        let policy_source_machine = sqlx::query_scalar::<_, i64>(
            "SELECT machine_id FROM sources WHERE id = 'source-policy-test'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(policy_source_machine, machine_id);

        let first_authority = PgPolicyRepository::for_machine(pool.clone(), machine_id);
        let second_authority = PgPolicyRepository::for_machine(pool.clone(), machine_id);
        let first_request = PolicyMutationRequest {
            source_id: "source-race-test".to_owned(),
            modality: "browser".to_owned(),
            expected_epoch: None,
            consent: true,
            excluded: false,
            retention_class: None,
            reason: "first concurrent mutation".to_owned(),
        };
        let second_request = PolicyMutationRequest {
            reason: "second concurrent mutation".to_owned(),
            ..first_request.clone()
        };
        let first = tokio::spawn(async move { first_authority.mutate_policy(first_request).await });
        let second = tokio::spawn(async move { second_authority.mutate_policy(second_request).await });
        let mut epochs = [first.await??.policy_epoch, second.await??.policy_epoch];
        epochs.sort_unstable();
        assert_eq!(epochs, [2, 3], "concurrent first writes must serialize");
        let audit_epochs = sqlx::query_scalar::<_, i64>(
            "SELECT new_epoch FROM policy_audit_log WHERE source_id = 'source-race-test' ORDER BY new_epoch",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(audit_epochs, vec![2, 3]);

        sqlx::query("INSERT INTO sources (id, machine_id, name) VALUES ('source-test', $1, 'Screen')")
            .bind(machine_id)
            .execute(&pool)
            .await?;
        let clipboard_consent = sqlx::query_scalar::<_, bool>(
            "INSERT INTO source_policies (source_id, modality) VALUES ('source-test', 'clipboard') RETURNING consent",
        )
        .fetch_one(&pool)
        .await?;
        assert!(!clipboard_consent, "clipboard must begin ungranted in PostgreSQL");

        sqlx::query(
            "INSERT INTO observations (id, source_id, modality, machine_id, status, observed_at) \
             VALUES ('observation-test', 'source-test', 'screen', $1, 'Available', now())",
        )
        .bind(machine_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO text_revisions (target_kind, target_id, revision_kind, content, content_hash, producer) \
             VALUES ('observation', 'observation-test', 'raw', 'synthetic text', 'hash-test', 'context_graph')",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO annotations (target_kind, target_id, annotation_type, content) \
             VALUES ('observation', 'observation-test', 'classification', 'synthetic')",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relations (subject_kind, subject_id, relation_type, object_kind, object_id) \
             VALUES ('source', 'source-test', 'produced', 'observation', 'observation-test')",
        )
        .execute(&pool)
        .await?;
        let retrieval_shape = sqlx::query(
            "SELECT revision.content, annotation.content, relation.relation_type \
             FROM text_revisions AS revision \
             JOIN annotations AS annotation ON annotation.target_id = revision.target_id \
             JOIN relations AS relation ON relation.object_id = revision.target_id \
             WHERE revision.target_id = 'observation-test'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(retrieval_shape.get::<String, _>(0), "synthetic text");
        assert_eq!(retrieval_shape.get::<String, _>(1), "synthetic");
        assert_eq!(retrieval_shape.get::<String, _>(2), "produced");

        sqlx::query("INSERT INTO browser_resources (source_id, browser_policy_epoch, observed_at) VALUES ('source-test', 1, '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await?;
        let duplicate_browser = sqlx::query("INSERT INTO browser_resources (source_id, browser_policy_epoch, observed_at) VALUES ('source-test', 1, '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await;
        assert!(duplicate_browser.is_err(), "BrowserRequestKey must be unique");

        let invalid_status = sqlx::query(
            "INSERT INTO observations (id, source_id, modality, machine_id, status, observed_at) VALUES ('bad-status', 'source-test', 'screen', $1, 'unknown', now())",
        )
        .bind(machine_id)
        .execute(&pool)
        .await;
        assert!(invalid_status.is_err(), "observation status must be constrained");

        let raw_media_columns = sqlx::query(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND (column_name ILIKE '%frame%' OR column_name ILIKE '%audio%' OR column_name ILIKE '%media%')",
        )
        .fetch_all(&pool)
        .await?;
        assert!(
            raw_media_columns.is_empty(),
            "v3 schema must not add raw-media columns: {:?}",
            raw_media_columns
                .iter()
                .map(|row| row.get::<String, _>("column_name"))
                .collect::<Vec<_>>()
        );

        Ok(())
    }
    .await;
    let cleanup = reset_v3_schema(&pool).await;
    pool.close().await;
    result.and(cleanup)
}
