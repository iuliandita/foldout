-- A rejection permanently supersedes every earlier selection in its scope, so revoking the
-- rejection never revives them. Only a selection recorded after the revocation authorizes.
ALTER TABLE release_decisions ADD COLUMN superseded_by TEXT
    REFERENCES release_decisions(id) ON DELETE RESTRICT
    CHECK (superseded_by IS NULL OR action = 'selected');

-- Legacy rows only have second-granularity timestamps; ties fail closed.
UPDATE release_decisions AS s SET superseded_by = (
    SELECT r.id FROM release_decisions r
    JOIN release_assessments ra ON ra.id = r.assessment_id
    JOIN release_assessments sa ON sa.id = s.assessment_id
    WHERE r.action = 'rejected' AND r.owner = s.owner AND ra.owner = sa.owner
      AND ra.unit_id = sa.unit_id AND ra.policy_fingerprint = sa.policy_fingerprint
      AND ra.integration_id = sa.integration_id AND ra.indexer_id = sa.indexer_id
      AND ra.guid_digest = sa.guid_digest AND ra.content_type = sa.content_type
      AND ra.protocol = sa.protocol AND r.created_at >= s.created_at
    ORDER BY r.created_at, r.id LIMIT 1
)
WHERE s.action = 'selected';

CREATE INDEX release_decisions_superseded_by ON release_decisions(superseded_by)
    WHERE superseded_by IS NOT NULL;
