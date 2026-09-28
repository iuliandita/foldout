-- Rows created before this migration have no recorded time and sort as oldest (0).
ALTER TABLE publications ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0 CHECK (created_at >= 0);
CREATE INDEX publications_created_at_id ON publications (created_at, id);
CREATE INDEX publications_content_type_created_at_id ON publications (content_type, created_at, id);
