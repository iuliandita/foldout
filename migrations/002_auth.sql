CREATE TABLE auth_users (
    id TEXT PRIMARY KEY,
    singleton INTEGER NOT NULL DEFAULT 1 UNIQUE CHECK (singleton = 1),
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL
);

CREATE TABLE auth_sessions (
    token_digest TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX auth_sessions_expiry ON auth_sessions(expires_at);

CREATE TABLE auth_keys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'manage', 'admin')),
    token_digest TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
