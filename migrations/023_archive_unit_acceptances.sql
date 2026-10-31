CREATE TABLE archive_unit_acceptances (
    owner TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    request_fingerprint TEXT NOT NULL CHECK (length(request_fingerprint) = 64),
    response_json TEXT NOT NULL CHECK (json_valid(response_json)),
    PRIMARY KEY (idempotency_key)
);
