ALTER TABLE scan_entries ADD COLUMN hints_signature TEXT;

CREATE TABLE identity_hints (
    entry_id TEXT NOT NULL REFERENCES scan_entries(id) ON DELETE CASCADE,
    library_file_id TEXT REFERENCES library_files(id) ON DELETE SET NULL,
    signature TEXT NOT NULL,
    kind TEXT NOT NULL,
    raw_value TEXT NOT NULL,
    value TEXT,
    status TEXT NOT NULL CHECK (status IN ('usable', 'needs_review')),
    source_field TEXT NOT NULL,
    container_path TEXT NOT NULL,
    extracted_at INTEGER NOT NULL
);
CREATE INDEX identity_hints_entry ON identity_hints(entry_id);
CREATE INDEX identity_hints_file ON identity_hints(library_file_id);
CREATE TRIGGER identity_hints_link_file AFTER INSERT ON library_files
BEGIN
    UPDATE identity_hints SET library_file_id = NEW.id
    WHERE signature = NEW.signature AND entry_id IN (
        SELECT id FROM scan_entries WHERE root_id = NEW.root_id AND relative_path = NEW.relative_path AND signature = NEW.signature
    );
END;
