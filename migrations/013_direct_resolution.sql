-- External triggers must be absent while their referenced table is rebuilt.
DROP TRIGGER indexer_excludes_direct;
DROP TRIGGER indexer_association_excludes_direct;
DROP TRIGGER direct_excludes_indexer;
DROP TRIGGER direct_manifest_immutable;
DROP TRIGGER direct_chapter_fence;
CREATE TABLE direct_acquisitions_new (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    link_handle TEXT NOT NULL REFERENCES direct_selections(handle) ON DELETE RESTRICT,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    download_root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_root_id TEXT NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    destination_relative TEXT NOT NULL,
    import_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('queued','downloading','downloaded','importing','completed','needs_review','canceled')),
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
    manifest_identity TEXT CHECK (manifest_identity IS NULL OR length(manifest_identity)=64),
    expected_pages INTEGER CHECK (expected_pages IS NULL OR expected_pages BETWEEN 1 AND 1000),
    canceled_at INTEGER,
    CHECK ((state = 'canceled') = (canceled_at IS NOT NULL)),
    UNIQUE(owner, idempotency_key)
);
INSERT INTO direct_acquisitions_new (id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,downloaded_bytes,content_digest,source_identity,root_identity,destination_identity,source_relative,created_at,updated_at,manifest_identity,expected_pages) SELECT id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,downloaded_bytes,content_digest,source_identity,root_identity,destination_identity,source_relative,created_at,updated_at,manifest_identity,expected_pages FROM direct_acquisitions;
DROP TABLE direct_acquisitions;
ALTER TABLE direct_acquisitions_new RENAME TO direct_acquisitions;
CREATE INDEX direct_due ON direct_acquisitions(state, created_at);
CREATE UNIQUE INDEX direct_active_unit ON direct_acquisitions(unit_id) WHERE state != 'canceled';
CREATE UNIQUE INDEX direct_active_destination ON direct_acquisitions(destination_root_id,destination_relative) WHERE state != 'canceled';
CREATE TRIGGER direct_excludes_indexer BEFORE INSERT ON direct_acquisitions
WHEN EXISTS(SELECT 1 FROM acquisition_runs WHERE state != 'canceled' AND
    (unit_id = NEW.unit_id OR (destination_root = NEW.destination_root_id AND destination_relative = NEW.destination_relative)))
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
CREATE TRIGGER indexer_excludes_direct BEFORE INSERT ON acquisition_runs
WHEN EXISTS(SELECT 1 FROM direct_acquisitions WHERE state != 'canceled' AND (unit_id = NEW.unit_id OR
    (destination_root_id = NEW.destination_root AND destination_relative = NEW.destination_relative)))
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
CREATE TRIGGER indexer_association_excludes_direct BEFORE UPDATE OF destination_root, destination_relative ON acquisition_runs
WHEN EXISTS(SELECT 1 FROM direct_acquisitions WHERE state != 'canceled' AND destination_root_id = NEW.destination_root AND destination_relative = NEW.destination_relative)
BEGIN SELECT RAISE(ABORT, 'acquisition reservation conflict'); END;
CREATE TRIGGER direct_manifest_immutable BEFORE UPDATE OF manifest_identity,expected_pages ON direct_acquisitions
WHEN OLD.attempted=1 AND (NEW.manifest_identity IS NOT OLD.manifest_identity OR NEW.expected_pages IS NOT OLD.expected_pages)
BEGIN SELECT RAISE(ABORT, 'direct manifest is immutable'); END;
CREATE TRIGGER direct_chapter_fence BEFORE UPDATE OF attempted ON direct_acquisitions
WHEN NEW.attempted=1 AND EXISTS(SELECT 1 FROM direct_selections WHERE handle=NEW.link_handle AND source_kind='mangadex_chapter')
  AND (NEW.manifest_identity IS NULL OR NEW.expected_pages IS NULL
       OR NEW.expected_pages IS NOT (SELECT page_count FROM direct_chapter_selections WHERE handle=NEW.link_handle))
BEGIN SELECT RAISE(ABORT, 'chapter fence requires complete manifest evidence'); END;
CREATE TRIGGER direct_canceled_terminal BEFORE UPDATE ON direct_acquisitions
WHEN OLD.state='canceled'
BEGIN SELECT RAISE(ABORT, 'canceled direct intent is immutable'); END;
