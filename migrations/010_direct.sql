-- Only origin-bound post paths and link digests are durable; never download URLs.
CREATE TABLE direct_selections (
    handle TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    integration_id TEXT NOT NULL REFERENCES integrations(id) ON DELETE RESTRICT,
    source_fingerprint TEXT NOT NULL,
    post_path TEXT NOT NULL,
    link_digest TEXT NOT NULL DEFAULT '',
    UNIQUE(owner, integration_id, source_fingerprint, post_path, link_digest)
);
CREATE TABLE direct_acquisitions (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    link_handle TEXT NOT NULL REFERENCES direct_selections(handle) ON DELETE RESTRICT,
    unit_id TEXT NOT NULL UNIQUE REFERENCES units(id) ON DELETE RESTRICT,
    download_root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_relative TEXT NOT NULL,
    import_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('queued','downloading','downloaded','importing','completed','needs_review')),
    reason TEXT,
    attempted INTEGER NOT NULL DEFAULT 0 CHECK (attempted IN (0,1)),
    downloaded_bytes INTEGER NOT NULL DEFAULT 0 CHECK (downloaded_bytes BETWEEN 0 AND 2147483648),
    content_digest TEXT,
    source_identity TEXT,
    root_identity TEXT,
    destination_identity TEXT,
    source_relative TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(owner, idempotency_key),
    UNIQUE(destination_root_id, destination_relative)
);
CREATE INDEX direct_due ON direct_acquisitions(state, created_at);
CREATE TABLE direct_worker_lock (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    token TEXT,
    until_at INTEGER NOT NULL DEFAULT 0
);
INSERT INTO direct_worker_lock(id) VALUES (1);
-- Reservations also exclude the indexer pipeline, including later file association.
CREATE TRIGGER direct_excludes_indexer BEFORE INSERT ON direct_acquisitions
WHEN EXISTS(SELECT 1 FROM acquisition_runs WHERE state != 'canceled' AND
    (unit_id = NEW.unit_id OR (destination_root = NEW.destination_root_id AND destination_relative = NEW.destination_relative)))
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
CREATE TRIGGER indexer_excludes_direct BEFORE INSERT ON acquisition_runs
WHEN EXISTS(SELECT 1 FROM direct_acquisitions WHERE unit_id = NEW.unit_id OR
    (destination_root_id = NEW.destination_root AND destination_relative = NEW.destination_relative))
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
CREATE TRIGGER indexer_association_excludes_direct BEFORE UPDATE OF destination_root, destination_relative ON acquisition_runs
WHEN EXISTS(SELECT 1 FROM direct_acquisitions WHERE destination_root_id = NEW.destination_root AND destination_relative = NEW.destination_relative)
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
