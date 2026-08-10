-- v3 context-pipeline additive schema (Postgres 18+)
-- Mirrors the authoritative schema in
-- pieces-memory-observations/docs/system/schema/v3-context-pipeline.sql.
--
-- INVARIANT: No raw media or binary audio payload columns exist in relational tables.

BEGIN;

CREATE TABLE IF NOT EXISTS sources (
    id TEXT PRIMARY KEY,
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS sources_machine_id_idx ON sources (machine_id);

CREATE TABLE IF NOT EXISTS source_policies (
    source_id TEXT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    modality TEXT NOT NULL,
    consent BOOLEAN NOT NULL DEFAULT FALSE,
    excluded BOOLEAN NOT NULL DEFAULT FALSE,
    retention_class TEXT NOT NULL DEFAULT 'default',
    policy_epoch BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (source_id, modality),
    CONSTRAINT source_policies_epoch_positive CHECK (policy_epoch >= 1),
    CONSTRAINT source_policies_modality_check CHECK (modality IN ('screen', 'browser', 'clipboard', 'audio'))
);

CREATE INDEX IF NOT EXISTS source_policies_lookup_idx ON source_policies (source_id, modality);

CREATE TABLE IF NOT EXISTS policy_audit_log (
    id BIGSERIAL PRIMARY KEY,
    source_id TEXT NOT NULL,
    modality TEXT NOT NULL,
    previous_epoch BIGINT NOT NULL,
    new_epoch BIGINT NOT NULL,
    consent BOOLEAN NOT NULL,
    excluded BOOLEAN NOT NULL,
    retention_class TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    mutated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT policy_audit_log_epoch_order CHECK (new_epoch > previous_epoch),
    CONSTRAINT policy_audit_log_modality_check CHECK (modality IN ('screen', 'browser', 'clipboard', 'audio'))
);

CREATE INDEX IF NOT EXISTS policy_audit_log_source_idx ON policy_audit_log (source_id, modality);
CREATE UNIQUE INDEX IF NOT EXISTS policy_audit_log_source_epoch_uidx ON policy_audit_log (source_id, modality, new_epoch);

CREATE TABLE IF NOT EXISTS windows (
    id BIGSERIAL PRIMARY KEY,
    machine_id BIGINT REFERENCES machines (id),
    window_key TEXT NOT NULL,
    hwnd BIGINT,
    window_generation BIGINT,
    app_id BIGINT REFERENCES apps (id),
    title TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS windows_machine_window_key_uidx ON windows (coalesce(machine_id, -1), window_key);
CREATE INDEX IF NOT EXISTS windows_hwnd_gen_idx ON windows (hwnd, window_generation);

CREATE TABLE IF NOT EXISTS browser_resources (
    id BIGSERIAL PRIMARY KEY,
    source_id TEXT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    hwnd BIGINT,
    window_generation BIGINT,
    browser_policy_epoch BIGINT NOT NULL DEFAULT 1,
    url TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    domain TEXT NOT NULL DEFAULT '',
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS browser_resources_source_observed_idx ON browser_resources (source_id, observed_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS browser_resources_request_key_uidx ON browser_resources (source_id, coalesce(hwnd, -1), coalesce(window_generation, -1), browser_policy_epoch, observed_at);

CREATE TABLE IF NOT EXISTS applications (
    id BIGSERIAL PRIMARY KEY,
    app_id BIGINT REFERENCES apps (id),
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    app_key TEXT NOT NULL,
    app_title TEXT NOT NULL DEFAULT '',
    bundle_id TEXT NOT NULL DEFAULT '',
    executable_path TEXT NOT NULL DEFAULT '',
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT applications_machine_app_key_uidx UNIQUE (machine_id, app_key)
);

CREATE INDEX IF NOT EXISTS applications_machine_idx ON applications (machine_id);

CREATE TABLE IF NOT EXISTS sessions (
    id BIGSERIAL PRIMARY KEY,
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    session_key TEXT NOT NULL UNIQUE,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sessions_ended_after_started CHECK (ended_at IS NULL OR ended_at >= started_at)
);

CREATE INDEX IF NOT EXISTS sessions_machine_started_idx ON sessions (machine_id, started_at DESC);

CREATE TABLE IF NOT EXISTS projects (
    id BIGSERIAL PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS actors (
    id BIGSERIAL PRIMARY KEY,
    actor_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT '',
    email TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS observations (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    modality TEXT NOT NULL,
    machine_id BIGINT NOT NULL REFERENCES machines (id),
    policy_epoch BIGINT NOT NULL DEFAULT 1,
    window_key TEXT NOT NULL DEFAULT 'none',
    status TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    producer TEXT NOT NULL DEFAULT '',
    producer_version TEXT NOT NULL DEFAULT '',
    payload_meta JSONB NOT NULL DEFAULT '{}'::jsonb,
    observed_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT observations_status_check CHECK (status IN ('Available', 'Absent', 'Denied', 'Failed', 'TimedOut', 'Cancelled', 'Stale')),
    CONSTRAINT observations_modality_check CHECK (modality IN ('screen', 'browser', 'clipboard', 'audio'))
);

CREATE INDEX IF NOT EXISTS observations_source_modality_idx ON observations (source_id, modality, observed_at DESC);

CREATE TABLE IF NOT EXISTS gaps (
    id BIGSERIAL PRIMARY KEY,
    schema_version INT NOT NULL DEFAULT 1,
    source_id TEXT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    modality TEXT NOT NULL,
    window_key TEXT NOT NULL DEFAULT 'none',
    reason TEXT NOT NULL,
    request_id TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT gaps_window_order CHECK (ended_at >= started_at),
    CONSTRAINT gaps_modality_check CHECK (modality IN ('screen', 'browser', 'clipboard', 'audio'))
);

CREATE INDEX IF NOT EXISTS gaps_source_started_idx ON gaps (source_id, modality, started_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS gaps_replay_uidx ON gaps (schema_version, source_id, modality, window_key, reason, coalesce(request_id, ''), started_at);

CREATE TABLE IF NOT EXISTS text_revisions (
    id BIGSERIAL PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    revision_kind TEXT NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    producer TEXT NOT NULL,
    model TEXT,
    policy_epoch BIGINT,
    is_accepted BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT text_revisions_kind_check CHECK (revision_kind IN ('raw', 'readable', 'description', 'title'))
);

CREATE INDEX IF NOT EXISTS text_revisions_target_idx ON text_revisions (target_kind, target_id);
CREATE UNIQUE INDEX IF NOT EXISTS text_revisions_accepted_uidx ON text_revisions (target_kind, target_id, revision_kind) WHERE (is_accepted = TRUE);

CREATE TABLE IF NOT EXISTS annotations (
    id BIGSERIAL PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    annotation_type TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS annotations_target_idx ON annotations (target_kind, target_id);

CREATE TABLE IF NOT EXISTS relations (
    id BIGSERIAL PRIMARY KEY,
    subject_kind TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    relation_type TEXT NOT NULL,
    object_kind TEXT NOT NULL,
    object_id TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS relations_subject_idx ON relations (subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS relations_object_idx ON relations (object_kind, object_id);

CREATE TABLE IF NOT EXISTS embeddings (
    id BIGSERIAL PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    text_revision_id BIGINT NOT NULL REFERENCES text_revisions (id) ON DELETE CASCADE,
    model_version TEXT NOT NULL,
    dimensions INT NOT NULL,
    vector_data BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS embeddings_target_idx ON embeddings (target_kind, target_id);

CREATE TABLE IF NOT EXISTS reconstruction_jobs (
    id BIGSERIAL PRIMARY KEY,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    reason TEXT NOT NULL DEFAULT '',
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT reconstruction_jobs_status_check CHECK (status IN ('pending', 'running', 'completed', 'failed'))
);

CREATE INDEX IF NOT EXISTS reconstruction_jobs_status_idx ON reconstruction_jobs (status);
CREATE UNIQUE INDEX IF NOT EXISTS reconstruction_jobs_scope_active_uidx ON reconstruction_jobs (scope_kind, scope_id) WHERE status IN ('pending', 'running');

CREATE TABLE IF NOT EXISTS retention_metadata (
    id BIGSERIAL PRIMARY KEY,
    entity_kind TEXT NOT NULL,
    policy_name TEXT NOT NULL,
    retention_days INT NOT NULL DEFAULT 30,
    auto_delete BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT retention_metadata_days_non_negative CHECK (retention_days >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS retention_metadata_entity_policy_uidx ON retention_metadata (entity_kind, policy_name);

COMMIT;
