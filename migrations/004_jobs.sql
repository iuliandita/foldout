CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL,
    payload TEXT NOT NULL,
    payload_version INTEGER NOT NULL DEFAULT 1,
    payload_fingerprint TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'retry_wait', 'needs_review', 'completed', 'failed', 'cancel_requested', 'canceled')),
    attempts INTEGER NOT NULL DEFAULT 0,
    claim_base INTEGER NOT NULL DEFAULT 0,
    worker TEXT,
    lease_until INTEGER,
    retry_at INTEGER,
    reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX jobs_active_dedup ON jobs(kind, scope, payload_fingerprint)
WHERE state IN ('queued', 'running', 'retry_wait', 'needs_review', 'cancel_requested');
CREATE INDEX jobs_claimable ON jobs(state, retry_at, created_at);
CREATE INDEX jobs_list_page ON jobs(created_at DESC, id DESC);

CREATE TABLE job_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL REFERENCES jobs(id),
    event TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT NOT NULL,
    worker TEXT,
    reason TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX job_events_by_job ON job_events(job_id, id);

CREATE TABLE acquisition_intents (
    id TEXT PRIMARY KEY,
    caller TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    job_id TEXT NOT NULL UNIQUE REFERENCES jobs(id),
    created_at INTEGER NOT NULL,
    UNIQUE(caller, idempotency_key)
);
