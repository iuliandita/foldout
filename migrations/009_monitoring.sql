-- Fixed intervals in UTC Unix seconds, aligned to the epoch. No cron or IANA zones.
CREATE TABLE monitors (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    integration_id TEXT NOT NULL REFERENCES integrations(id) ON DELETE RESTRICT,
    content_type TEXT NOT NULL CHECK (content_type IN ('comic', 'manga', 'magazine')),
    query TEXT NOT NULL CHECK (length(trim(query)) BETWEEN 1 AND 512),
    interval_seconds INTEGER NOT NULL CHECK (interval_seconds BETWEEN 900 AND 31536000),
    selection_policy TEXT NOT NULL CHECK (selection_policy = 'review_only'),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    next_run INTEGER NOT NULL CHECK (next_run >= 0),
    last_run_at INTEGER,
    last_state TEXT NOT NULL CHECK (last_state IN ('scheduled', 'disabled', 'awaiting_release', 'needs_review', 'source_error', 'canceled')),
    reason TEXT,
    candidates TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(candidates) AND length(candidates) <= 262144),
    candidates_truncated INTEGER NOT NULL DEFAULT 0 CHECK (candidates_truncated IN (0, 1)),
    lease_token TEXT,
    lease_until INTEGER,
    deleted_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((lease_token IS NULL) = (lease_until IS NULL)),
    CHECK (deleted_at IS NULL OR enabled = 0)
);
CREATE UNIQUE INDEX monitors_scope ON monitors(owner, unit_id, integration_id) WHERE deleted_at IS NULL;
CREATE INDEX monitors_owner_page ON monitors(owner, id) WHERE deleted_at IS NULL;
CREATE INDEX monitors_due ON monitors(enabled, next_run, id) WHERE deleted_at IS NULL;
CREATE INDEX monitors_scope_lease ON monitors(owner, unit_id, integration_id, lease_until);
-- Candidates contain only sanitized search DTO fields and expiring opaque handles.
-- They intentionally do not FK search_releases: the shared search expiry sweep owns handles.
-- Deleted rows retain their lease until completion/expiry so replacement scopes cannot overlap.
