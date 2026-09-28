CREATE TABLE release_selection_policies (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL CHECK (revision > 0),
    mode TEXT NOT NULL CHECK (mode = 'review_only'),
    format_order TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(format_order) AND json_type(format_order) = 'array'),
    source_priority TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(source_priority) AND json_type(source_priority) = 'array'),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(owner, unit_id, revision)
);
CREATE INDEX release_selection_policies_owner_unit ON release_selection_policies(owner, unit_id, revision DESC);

CREATE TABLE release_assessments (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    release_handle TEXT NOT NULL,
    integration_id TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL CHECK (length(source_fingerprint) = 64 AND source_fingerprint NOT GLOB '*[^0-9a-f]*'),
    indexer_id INTEGER NOT NULL CHECK (indexer_id BETWEEN 0 AND 4294967295),
    guid_digest TEXT NOT NULL CHECK (length(guid_digest) = 64 AND guid_digest NOT GLOB '*[^0-9a-f]*'),
    content_type TEXT NOT NULL CHECK (content_type IN ('comic', 'manga', 'magazine')),
    protocol TEXT NOT NULL CHECK (protocol IN ('usenet', 'torrent')),
    policy_id TEXT NOT NULL REFERENCES release_selection_policies(id) ON DELETE RESTRICT,
    policy_fingerprint TEXT NOT NULL CHECK (length(policy_fingerprint) = 64 AND policy_fingerprint NOT GLOB '*[^0-9a-f]*'),
    target_fingerprint TEXT NOT NULL CHECK (length(target_fingerprint) = 64 AND target_fingerprint NOT GLOB '*[^0-9a-f]*'),
    evidence_fingerprint TEXT NOT NULL CHECK (length(evidence_fingerprint) = 64 AND evidence_fingerprint NOT GLOB '*[^0-9a-f]*'),
    evidence_json TEXT NOT NULL CHECK (length(evidence_json) <= 32768 AND json_valid(evidence_json) AND json_type(evidence_json) = 'object'),
    reasons_json TEXT NOT NULL CHECK (length(reasons_json) <= 16384 AND json_valid(reasons_json) AND json_type(reasons_json) = 'array'),
    eligibility TEXT NOT NULL CHECK (eligibility IN ('eligible', 'unknown', 'ineligible')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at)
);
CREATE INDEX release_assessments_identity_expiry ON release_assessments(owner, unit_id, policy_id, integration_id, source_fingerprint, indexer_id, guid_digest, content_type, protocol, policy_fingerprint, target_fingerprint, evidence_fingerprint, expires_at);
CREATE INDEX release_assessments_owner_id ON release_assessments(owner, id);
CREATE INDEX release_assessments_expiry ON release_assessments(expires_at, id);
CREATE INDEX release_assessments_rejection_scope ON release_assessments(owner, unit_id, policy_fingerprint, integration_id, indexer_id, guid_digest, content_type, protocol);

CREATE TABLE release_decisions (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    assessment_id TEXT NOT NULL REFERENCES release_assessments(id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN ('selected', 'rejected')),
    reason TEXT CHECK (reason IS NULL OR length(reason) <= 512),
    acknowledged_assessment_id TEXT REFERENCES release_assessments(id) ON DELETE RESTRICT,
    idempotency_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL CHECK (length(request_fingerprint) = 64 AND request_fingerprint NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(owner, idempotency_key)
);
CREATE INDEX release_decisions_owner_assessment ON release_decisions(owner, assessment_id, action);
CREATE INDEX release_decisions_assessment ON release_decisions(assessment_id);
CREATE INDEX release_decisions_acknowledged_assessment ON release_decisions(acknowledged_assessment_id) WHERE acknowledged_assessment_id IS NOT NULL;

CREATE TABLE release_rejection_revocations (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    rejection_decision_id TEXT NOT NULL UNIQUE REFERENCES release_decisions(id) ON DELETE RESTRICT,
    idempotency_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL CHECK (length(request_fingerprint) = 64 AND request_fingerprint NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(owner, idempotency_key)
);

ALTER TABLE acquisition_runs ADD COLUMN selection_decision_id TEXT REFERENCES release_decisions(id) ON DELETE RESTRICT;
