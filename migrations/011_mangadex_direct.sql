ALTER TABLE direct_selections ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'getcomics' CHECK (source_kind IN ('getcomics','mangadex_chapter'));
CREATE TABLE direct_chapter_selections (
    handle TEXT PRIMARY KEY REFERENCES direct_selections(handle) ON DELETE RESTRICT,
    chapter_id TEXT NOT NULL,
    manga_id TEXT NOT NULL,
    language TEXT NOT NULL,
    metadata_identity TEXT NOT NULL CHECK (length(metadata_identity)=64),
    page_count INTEGER NOT NULL CHECK (page_count BETWEEN 0 AND 1000)
);
ALTER TABLE direct_acquisitions ADD COLUMN manifest_identity TEXT CHECK (manifest_identity IS NULL OR length(manifest_identity)=64);
ALTER TABLE direct_acquisitions ADD COLUMN expected_pages INTEGER CHECK (expected_pages IS NULL OR expected_pages BETWEEN 1 AND 1000);
CREATE TRIGGER direct_manifest_immutable BEFORE UPDATE OF manifest_identity,expected_pages ON direct_acquisitions
WHEN OLD.attempted=1 AND (NEW.manifest_identity IS NOT OLD.manifest_identity OR NEW.expected_pages IS NOT OLD.expected_pages)
BEGIN SELECT RAISE(ABORT, 'direct manifest is immutable'); END;
CREATE TRIGGER direct_chapter_immutable BEFORE UPDATE ON direct_chapter_selections
BEGIN SELECT RAISE(ABORT, 'direct chapter selection is immutable'); END;
CREATE TRIGGER direct_selection_identity_immutable BEFORE UPDATE ON direct_selections
WHEN NEW.owner IS NOT OLD.owner OR NEW.integration_id IS NOT OLD.integration_id
  OR NEW.source_fingerprint IS NOT OLD.source_fingerprint OR NEW.post_path IS NOT OLD.post_path
  OR NEW.link_digest IS NOT OLD.link_digest OR NEW.source_kind IS NOT OLD.source_kind
BEGIN SELECT RAISE(ABORT, 'direct selection is immutable'); END;
CREATE TRIGGER direct_chapter_fence BEFORE UPDATE OF attempted ON direct_acquisitions
WHEN NEW.attempted=1 AND EXISTS(SELECT 1 FROM direct_selections WHERE handle=NEW.link_handle AND source_kind='mangadex_chapter')
  AND (NEW.manifest_identity IS NULL OR NEW.expected_pages IS NULL
       OR NEW.expected_pages IS NOT (SELECT page_count FROM direct_chapter_selections WHERE handle=NEW.link_handle))
BEGIN SELECT RAISE(ABORT, 'chapter fence requires complete manifest evidence'); END;
