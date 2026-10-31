-- No URLs: only manifest page identities and validator-bound fsynced checkpoints.
CREATE TABLE direct_review_revisions (
    acquisition_id TEXT PRIMARY KEY REFERENCES direct_acquisitions(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0)
);
INSERT INTO direct_review_revisions(acquisition_id) SELECT id FROM direct_acquisitions;
CREATE TRIGGER direct_review_initial AFTER INSERT ON direct_acquisitions
BEGIN
    INSERT INTO direct_review_revisions(acquisition_id) VALUES(NEW.id);
END;
CREATE TRIGGER direct_review_revision AFTER UPDATE ON direct_acquisitions
BEGIN
    UPDATE direct_review_revisions SET revision=revision+1 WHERE acquisition_id=NEW.id;
END;
CREATE TABLE direct_transfer_parts (
    acquisition_id TEXT NOT NULL REFERENCES direct_acquisitions(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 0 AND 1000),
    page_identity TEXT NOT NULL CHECK (length(page_identity)=64),
    checkpoint TEXT CHECK (checkpoint IS NULL OR json_valid(checkpoint)),
    PRIMARY KEY(acquisition_id, ordinal)
);
CREATE TRIGGER direct_part_revision AFTER UPDATE ON direct_transfer_parts
BEGIN
    UPDATE direct_review_revisions SET revision=revision+1 WHERE acquisition_id=NEW.acquisition_id;
END;
CREATE TRIGGER direct_part_insert_revision AFTER INSERT ON direct_transfer_parts
BEGIN
    UPDATE direct_review_revisions SET revision=revision+1 WHERE acquisition_id=NEW.acquisition_id;
END;
CREATE TRIGGER direct_part_delete_revision AFTER DELETE ON direct_transfer_parts
BEGIN
    UPDATE direct_review_revisions SET revision=revision+1 WHERE acquisition_id=OLD.acquisition_id;
END;
CREATE TABLE direct_transfer_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    acquisition_id TEXT NOT NULL REFERENCES direct_acquisitions(id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN ('resume','restart')),
    revision INTEGER NOT NULL,
    transfer_fingerprint TEXT NOT NULL,
    replacement_id TEXT REFERENCES direct_acquisitions(id) ON DELETE RESTRICT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(acquisition_id, revision)
);
