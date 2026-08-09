use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// Represents a committed snapshot of a modality-qualified policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub source_id: String,
    pub modality: String,
    pub consent: bool,
    pub excluded: bool,
    pub retention_class: String,
    pub policy_epoch: u64,
    pub updated_at: DateTime<Utc>,
}

/// Request to mutate a policy snapshot.
#[derive(Debug, Clone)]
pub struct PolicyMutationRequest {
    pub source_id: String,
    pub modality: String,
    pub expected_epoch: Option<u64>,
    pub consent: bool,
    pub excluded: bool,
    pub retention_class: Option<String>,
    pub reason: String,
}

/// Row-locking policy repository trait for database and in-memory policy authority.
#[async_trait::async_trait]
pub trait PolicyRepository: Send + Sync {
    /// Mutate a policy snapshot atomically with row-locking and epoch validation.
    async fn mutate_policy(&self, request: PolicyMutationRequest) -> Result<PolicySnapshot>;

    /// Retrieve the current committed policy snapshot for a (source_id, modality).
    async fn get_policy(&self, source_id: &str, modality: &str) -> Result<PolicySnapshot>;
}

/// In-memory policy authority for unit/integration testing with atomic row-level locks.
#[derive(Debug, Clone, Default)]
pub struct MemoryPolicyRepository {
    policies: Arc<Mutex<HashMap<(String, String), PolicySnapshot>>>,
    audit_log: Arc<Mutex<Vec<(PolicySnapshot, String)>>>,
}

impl MemoryPolicyRepository {
    pub fn new() -> Self {
        Self {
            policies: Arc::new(Mutex::new(HashMap::new())),
            audit_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl PolicyRepository for MemoryPolicyRepository {
    async fn mutate_policy(&self, request: PolicyMutationRequest) -> Result<PolicySnapshot> {
        let mut guard = self.policies.lock().unwrap();
        let key = (request.source_id.clone(), request.modality.clone());
        let now = Utc::now();

        let current = guard.entry(key.clone()).or_insert_with(|| {
            let default_consent = match request.modality.as_str() {
                "clipboard" | "audio" => false,
                _ => false, // Default off for all until granted
            };
            PolicySnapshot {
                source_id: request.source_id.clone(),
                modality: request.modality.clone(),
                consent: default_consent,
                excluded: false,
                retention_class: "default".to_string(),
                policy_epoch: 1,
                updated_at: now,
            }
        });

        if let Some(exp) = request.expected_epoch {
            if current.policy_epoch != exp {
                bail!(
                    "Policy epoch mismatch for ({}, {}): expected {}, found {}",
                    request.source_id,
                    request.modality,
                    exp,
                    current.policy_epoch
                );
            }
        }

        let new_epoch = current.policy_epoch + 1;
        let updated = PolicySnapshot {
            source_id: request.source_id.clone(),
            modality: request.modality.clone(),
            consent: request.consent,
            excluded: request.excluded,
            retention_class: request.retention_class.unwrap_or_else(|| current.retention_class.clone()),
            policy_epoch: new_epoch,
            updated_at: now,
        };

        *current = updated.clone();
        self.audit_log.lock().unwrap().push((updated.clone(), request.reason));
        Ok(updated)
    }

    async fn get_policy(&self, source_id: &str, modality: &str) -> Result<PolicySnapshot> {
        let guard = self.policies.lock().unwrap();
        let key = (source_id.to_string(), modality.to_string());
        if let Some(snapshot) = guard.get(&key) {
            Ok(snapshot.clone())
        } else {
            let default_consent = match modality {
                "clipboard" | "audio" => false,
                _ => false,
            };
            Ok(PolicySnapshot {
                source_id: source_id.to_string(),
                modality: modality.to_string(),
                consent: default_consent,
                excluded: false,
                retention_class: "default".to_string(),
                policy_epoch: 1,
                updated_at: Utc::now(),
            })
        }
    }
}

/// PostgreSQL implementation of `PolicyRepository` using `SELECT ... FOR UPDATE` row-locking.
#[derive(Debug, Clone)]
pub struct PgPolicyRepository {
    pool: PgPool,
}

impl PgPolicyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl PolicyRepository for PgPolicyRepository {
    async fn mutate_policy(&self, request: PolicyMutationRequest) -> Result<PolicySnapshot> {
        let mut tx = self.pool.begin().await.context("Failed to begin transaction for policy mutation")?;

        // 1. Ensure source exists
        sqlx::query(
            "INSERT INTO sources (id, name, kind) VALUES ($1, $1, 'custom') ON CONFLICT (id) DO NOTHING"
        )
        .bind(&request.source_id)
        .execute(&mut *tx)
        .await
        .context("Failed to ensure source existence")?;

        // 2. Lock row for update
        let existing = sqlx::query(
            "SELECT consent, excluded, retention_class, policy_epoch FROM source_policies WHERE source_id = $1 AND modality = $2 FOR UPDATE"
        )
        .bind(&request.source_id)
        .bind(&request.modality)
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to row-lock source policy")?;

        let (_cur_consent, _cur_excluded, cur_retention, cur_epoch) = match existing {
            Some(row) => (
                row.get::<bool, _>("consent"),
                row.get::<bool, _>("excluded"),
                row.get::<String, _>("retention_class"),
                row.get::<i64, _>("policy_epoch") as u64,
            ),
            None => {
                let default_consent = match request.modality.as_str() {
                    "clipboard" | "audio" => false,
                    _ => false,
                };
                (default_consent, false, "default".to_string(), 1u64)
            }
        };

        if let Some(exp) = request.expected_epoch {
            if cur_epoch != exp {
                bail!(
                    "Policy epoch mismatch for ({}, {}): expected {}, found {}",
                    request.source_id,
                    request.modality,
                    exp,
                    cur_epoch
                );
            }
        }

        let new_epoch = if cur_epoch > 0 { cur_epoch + 1 } else { 1 };
        let new_retention = request.retention_class.unwrap_or(cur_retention);

        // 3. Upsert policy row
        let row = sqlx::query(
            r#"
            INSERT INTO source_policies (source_id, modality, consent, excluded, retention_class, policy_epoch, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW())
            ON CONFLICT (source_id, modality) DO UPDATE
            SET consent = EXCLUDED.consent,
                excluded = EXCLUDED.excluded,
                retention_class = EXCLUDED.retention_class,
                policy_epoch = EXCLUDED.policy_epoch,
                updated_at = NOW()
            RETURNING source_id, modality, consent, excluded, retention_class, policy_epoch, updated_at
            "#
        )
        .bind(&request.source_id)
        .bind(&request.modality)
        .bind(request.consent)
        .bind(request.excluded)
        .bind(&new_retention)
        .bind(new_epoch as i64)
        .fetch_one(&mut *tx)
        .await
        .context("Failed to upsert source policy")?;

        // 4. Record audit entry atomically
        sqlx::query(
            r#"
            INSERT INTO policy_audit_log (source_id, modality, previous_epoch, new_epoch, consent, excluded, retention_class, reason)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#
        )
        .bind(&request.source_id)
        .bind(&request.modality)
        .bind(cur_epoch as i64)
        .bind(new_epoch as i64)
        .bind(request.consent)
        .bind(request.excluded)
        .bind(row.get::<String, _>("retention_class"))
        .bind(&request.reason)
        .execute(&mut *tx)
        .await
        .context("Failed to write policy audit log")?;

        tx.commit().await.context("Failed to commit policy transaction")?;

        Ok(PolicySnapshot {
            source_id: row.get::<String, _>("source_id"),
            modality: row.get::<String, _>("modality"),
            consent: row.get::<bool, _>("consent"),
            excluded: row.get::<bool, _>("excluded"),
            retention_class: row.get::<String, _>("retention_class"),
            policy_epoch: row.get::<i64, _>("policy_epoch") as u64,
            updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
        })
    }

    async fn get_policy(&self, source_id: &str, modality: &str) -> Result<PolicySnapshot> {
        let row = sqlx::query(
            "SELECT source_id, modality, consent, excluded, retention_class, policy_epoch, updated_at FROM source_policies WHERE source_id = $1 AND modality = $2"
        )
        .bind(source_id)
        .bind(modality)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to fetch source policy")?;

        if let Some(r) = row {
            Ok(PolicySnapshot {
                source_id: r.get::<String, _>("source_id"),
                modality: r.get::<String, _>("modality"),
                consent: r.get::<bool, _>("consent"),
                excluded: r.get::<bool, _>("excluded"),
                retention_class: r.get::<String, _>("retention_class"),
                policy_epoch: r.get::<i64, _>("policy_epoch") as u64,
                updated_at: r.get::<DateTime<Utc>, _>("updated_at"),
            })
        } else {
            let default_consent = match modality {
                "clipboard" | "audio" => false,
                _ => false,
            };
            Ok(PolicySnapshot {
                source_id: source_id.to_string(),
                modality: modality.to_string(),
                consent: default_consent,
                excluded: false,
                retention_class: "default".to_string(),
                policy_epoch: 1,
                updated_at: Utc::now(),
            })
        }
    }
}
