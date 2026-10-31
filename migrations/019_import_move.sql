CREATE TABLE import_operations_move (
    id TEXT PRIMARY KEY,
    source_root TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_root TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_relative TEXT NOT NULL,
    destination_path TEXT NOT NULL UNIQUE,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    request_json TEXT NOT NULL,
    identity_json TEXT NOT NULL,
    phase VARCHAR(32) NOT NULL CHECK (phase IN
        ('planned', 'staged', 'verified', 'finalized', 'cataloged', 'cleanup_pending', 'done')),
    effective_policy VARCHAR(16) CHECK (effective_policy IN ('copy', 'hardlink', 'move')),
    staged_identity_json TEXT,
    validation TEXT,
    library_file_id TEXT REFERENCES library_files(id) ON DELETE RESTRICT,
    last_error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (destination_root, destination_relative)
);
INSERT INTO import_operations_move SELECT * FROM import_operations;
DROP TABLE import_operations;
ALTER TABLE import_operations_move RENAME TO import_operations;
CREATE INDEX import_operations_phase ON import_operations(phase, created_at);
