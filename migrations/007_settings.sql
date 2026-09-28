CREATE TABLE integrations (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('comicvine', 'mangaupdates', 'mangadex', 'getcomics', 'prowlarr', 'sabnzbd', 'qbittorrent')),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
    base_url TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    options TEXT NOT NULL CHECK (json_valid(options) AND json_type(options) = 'object'),
    secret_version INTEGER NOT NULL CHECK (secret_version = 1),
    secret_nonce BLOB NOT NULL CHECK (length(secret_nonce) = 24),
    secret_ciphertext BLOB NOT NULL CHECK (length(secret_ciphertext) BETWEEN 28 AND 12316)
);
