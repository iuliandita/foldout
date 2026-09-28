CREATE TABLE library_roots (
    id TEXT PRIMARY KEY,
    label TEXT NOT NULL CHECK (length(trim(label)) > 0),
    path TEXT NOT NULL UNIQUE CHECK (length(trim(path)) > 0)
);

CREATE TABLE scan_runs (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'failed')),
    visited INTEGER NOT NULL DEFAULT 0 CHECK (visited >= 0),
    skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped >= 0),
    errors INTEGER NOT NULL DEFAULT 0 CHECK (errors >= 0),
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE scan_entries (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    relative_path TEXT NOT NULL,
    format TEXT CHECK (format IN ('cbz', 'cbr', 'pdf')),
    signature TEXT,
    size_bytes INTEGER,
    mtime_ns INTEGER,
    state TEXT NOT NULL CHECK (state IN ('pending_association', 'skipped', 'missing', 'error')),
    reason TEXT,
    last_seen_run_id TEXT REFERENCES scan_runs(id) ON DELETE SET NULL,
    UNIQUE (root_id, relative_path),
    CHECK (state != 'pending_association' OR (signature IS NOT NULL AND size_bytes IS NOT NULL AND mtime_ns IS NOT NULL))
);
CREATE INDEX scan_entries_root_state ON scan_entries (root_id, state);

ALTER TABLE library_files ADD COLUMN root_id TEXT REFERENCES library_roots(id) ON DELETE RESTRICT;
ALTER TABLE library_files ADD COLUMN relative_path TEXT;
ALTER TABLE library_files ADD COLUMN mtime_ns INTEGER;
CREATE UNIQUE INDEX library_files_root_relative_unique ON library_files (root_id, relative_path) WHERE root_id IS NOT NULL;

CREATE TABLE import_previews (
    id TEXT PRIMARY KEY,
    entry_id TEXT NOT NULL REFERENCES scan_entries(id) ON DELETE RESTRICT,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    signature TEXT NOT NULL,
    source_size INTEGER NOT NULL CHECK (source_size >= 0),
    source_mtime_ns INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'accepted', 'stale', 'conflict')),
    created_at INTEGER NOT NULL,
    accepted_library_file_id TEXT REFERENCES library_files(id) ON DELETE RESTRICT
);
CREATE INDEX import_previews_entry_id ON import_previews (entry_id);
