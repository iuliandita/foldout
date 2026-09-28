-- Provider GUIDs can themselves be credentialed URLs. Only their digests are stored.
CREATE TABLE search_releases (
    handle TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    integration_id TEXT NOT NULL REFERENCES integrations(id) ON DELETE RESTRICT,
    source_fingerprint TEXT NOT NULL,
    indexer_id INTEGER NOT NULL,
    guid_digest TEXT NOT NULL,
    content_type TEXT NOT NULL CHECK (content_type IN ('comic', 'manga', 'magazine')),
    protocol TEXT NOT NULL CHECK (protocol IN ('usenet', 'torrent')),
    query TEXT NOT NULL,
    search_offset INTEGER NOT NULL,
    search_limit INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    UNIQUE(owner, integration_id, source_fingerprint, indexer_id, guid_digest, content_type)
);
CREATE INDEX search_releases_expiry ON search_releases(expires_at);

CREATE TABLE search_cooldowns (
    scope TEXT PRIMARY KEY,
    next_at INTEGER NOT NULL
);

CREATE TABLE acquisition_runs (
    id TEXT PRIMARY KEY REFERENCES acquisition_intents(id) ON DELETE RESTRICT,
    release_handle TEXT NOT NULL REFERENCES search_releases(handle) ON DELETE RESTRICT,
    client_id TEXT NOT NULL REFERENCES integrations(id) ON DELETE RESTRICT,
    client_fingerprint TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK (client_kind IN ('sabnzbd', 'qbittorrent')),
    category TEXT NOT NULL,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('queued', 'downloading', 'downloaded', 'importing', 'completed', 'needs_review', 'canceled')),
    reason TEXT,
    attempted INTEGER NOT NULL DEFAULT 0 CHECK (attempted IN (0, 1)),
    payload_digest TEXT,
    torrent_hash TEXT,
    next_poll_at INTEGER NOT NULL DEFAULT 0,
    poll_cursor INTEGER NOT NULL DEFAULT 0,
    work_token TEXT,
    work_until INTEGER,
    import_id TEXT UNIQUE,
    source_root TEXT REFERENCES library_roots(id) ON DELETE RESTRICT,
    source_relative TEXT,
    destination_root TEXT REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_relative TEXT,
    destination_path TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((destination_root IS NULL) = (destination_relative IS NULL)),
    CHECK ((destination_root IS NULL) = (destination_path IS NULL)),
    CHECK ((source_root IS NULL) = (source_relative IS NULL)),
    CHECK (attempted = 0 OR payload_digest IS NOT NULL)
);
CREATE INDEX acquisition_due ON acquisition_runs(state, next_poll_at, created_at);
-- Permanent reservation: an uncertain add must never be resubmitted under another intent.
CREATE UNIQUE INDEX acquisition_torrent_fence ON acquisition_runs(client_id, torrent_hash)
    WHERE attempted = 1 AND torrent_hash IS NOT NULL;
CREATE UNIQUE INDEX acquisition_unit_reservation ON acquisition_runs(unit_id)
    WHERE state != 'canceled';
CREATE UNIQUE INDEX acquisition_destination_reservation ON acquisition_runs(destination_path)
    WHERE state != 'canceled' AND destination_path IS NOT NULL;

CREATE TABLE acquisition_receipts (
    acquisition_id TEXT NOT NULL REFERENCES acquisition_runs(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL,
    external_id TEXT NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    PRIMARY KEY(acquisition_id, ordinal),
    UNIQUE(acquisition_id, external_id)
);
