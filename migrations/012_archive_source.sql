-- All inbound references use RESTRICT. Preserve them while replacing the kind constraint.
PRAGMA defer_foreign_keys = ON;
CREATE TABLE integrations_new (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('comicvine', 'mangaupdates', 'mangadex', 'getcomics', 'prowlarr', 'sabnzbd', 'qbittorrent', 'internetarchive')),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
    base_url TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    options TEXT NOT NULL CHECK (json_valid(options) AND json_type(options) = 'object'),
    secret_version INTEGER NOT NULL CHECK (secret_version = 1),
    secret_nonce BLOB NOT NULL CHECK (length(secret_nonce) = 24),
    secret_ciphertext BLOB NOT NULL CHECK (length(secret_ciphertext) BETWEEN 28 AND 12316)
);
INSERT INTO integrations_new (id, kind, label, base_url, enabled, options, secret_version, secret_nonce, secret_ciphertext) SELECT id, kind, label, base_url, enabled, options, secret_version, secret_nonce, secret_ciphertext FROM integrations;
DROP TABLE integrations;
ALTER TABLE integrations_new RENAME TO integrations;
-- Check the complete schema before clearing SQLite's deferred drop-table counter.
CREATE TEMP TABLE archive_migration_fk_guard (valid INTEGER NOT NULL CHECK (valid = 1));
INSERT INTO archive_migration_fk_guard
SELECT NOT EXISTS (SELECT 1 FROM pragma_foreign_key_check);
DROP TABLE archive_migration_fk_guard;
PRAGMA defer_foreign_keys = OFF;
