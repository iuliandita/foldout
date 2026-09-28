ALTER TABLE search_releases ADD COLUMN evidence_json TEXT
    CHECK (evidence_json IS NULL OR (length(evidence_json) <= 32768
        AND json_valid(evidence_json) AND json_type(evidence_json) = 'object'));
