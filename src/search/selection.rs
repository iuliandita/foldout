use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;

use crate::{
    catalog::{
        ContentType,
        wanted::{CONTEXT_SQL, UnitContext, context_row},
    },
    jobs::store::validate_label,
    providers::{
        ReleaseProtocol,
        release_evidence::{Evidence, MediaFormat, ReleaseEvidence},
    },
    settings::{Integration, IntegrationKind, IntegrationOptions, ProwlarrOptions},
    store::sqlite::SqliteStore,
};

use super::{
    matching::{Candidate, Eligibility, Evaluation, evaluate},
    service::{digest, now, source_fingerprint},
};

#[path = "selection_decisions.rs"]
mod decisions;
pub use decisions::{Decision, DecisionAction, NewDecision, Revocation};

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("invalid selection request")]
    Invalid,
    #[error("selection record was not found")]
    NotFound,
    #[error("selection policy changed")]
    Changed,
    #[error("idempotency key or decision conflicts with an existing action")]
    Conflict,
    #[error("assessment has expired")]
    Expired,
    #[error("release conflicts with the selected unit")]
    Ineligible,
    #[error("acknowledge this exact assessment before selecting the release")]
    AcknowledgementRequired,
    #[error("release has an active rejection for this unit and policy")]
    Rejected,
    #[error("a later rejection superseded this selection; select the release again")]
    Superseded,
    #[error("selection database operation failed")]
    Database,
}

impl From<sqlx::Error> for SelectionError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Clone)]
pub struct SelectionRepository {
    store: SqliteStore,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDraft {
    pub format_order: Vec<MediaFormat>,
    pub source_priority: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionPolicy {
    pub id: String,
    pub unit_id: String,
    pub revision: i64,
    pub preferences: PolicyDraft,
    pub fingerprint: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseIdentity {
    pub handle: String,
    pub integration_id: String,
    pub source_fingerprint: String,
    pub indexer_id: u32,
    pub guid_digest: String,
    pub content_type: ContentType,
    pub protocol: ReleaseProtocol,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Assessment {
    pub id: String,
    pub unit_id: String,
    pub policy_id: String,
    pub policy_fingerprint: String,
    pub target_fingerprint: String,
    pub evidence_fingerprint: String,
    pub identity: ReleaseIdentity,
    pub evidence: ReleaseEvidence,
    pub evaluation: Evaluation,
    pub created_at: i64,
    pub expires_at: i64,
}

pub struct AssessmentInput<'a> {
    pub target: &'a UnitContext,
    pub policy_id: &'a str,
    pub identity: ReleaseIdentity,
    pub evidence: &'a ReleaseEvidence,
    pub evaluation: &'a Evaluation,
    pub expires_at: i64,
}

impl SelectionRepository {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn current_or_default_policy(
        &self,
        owner: &str,
        unit_id: &str,
    ) -> Result<SelectionPolicy, SelectionError> {
        validate_scope(owner, unit_id)?;
        let mut tx = self.store.begin_write().await?;
        let policy = match current_policy(&mut tx, owner, unit_id).await? {
            Some(policy) => policy,
            None => {
                require_unit(&mut tx, unit_id).await?;
                insert_policy(&mut tx, owner, unit_id, 1, PolicyDraft::default()).await?
            }
        };
        tx.commit().await?;
        Ok(policy)
    }

    pub async fn append_policy(
        &self,
        owner: &str,
        unit_id: &str,
        expected_revision: i64,
        draft: PolicyDraft,
    ) -> Result<SelectionPolicy, SelectionError> {
        validate_scope(owner, unit_id)?;
        validate_draft(&draft)?;
        let mut tx = self.store.begin_write().await?;
        let current = current_policy(&mut tx, owner, unit_id)
            .await?
            .ok_or(SelectionError::NotFound)?;
        if current.revision != expected_revision {
            return Err(SelectionError::Changed);
        }
        let policy = if current.preferences == draft {
            current
        } else {
            let revision = current
                .revision
                .checked_add(1)
                .ok_or(SelectionError::Invalid)?;
            insert_policy(&mut tx, owner, unit_id, revision, draft).await?
        };
        tx.commit().await?;
        Ok(policy)
    }

    pub async fn record_assessment(
        &self,
        owner: &str,
        input: AssessmentInput<'_>,
    ) -> Result<Assessment, SelectionError> {
        let mut tx = self.store.begin_write().await?;
        let assessment = Self::record_assessment_in(&mut tx, owner, input).await?;
        tx.commit().await?;
        Ok(assessment)
    }

    pub(crate) async fn record_assessment_in(
        connection: &mut SqliteConnection,
        owner: &str,
        input: AssessmentInput<'_>,
    ) -> Result<Assessment, SelectionError> {
        let AssessmentInput {
            target,
            policy_id,
            identity,
            evidence,
            evaluation,
            expires_at,
        } = input;
        validate_scope(owner, &target.unit.id)?;
        valid_id(policy_id)?;
        validate_identity(&identity)?;
        if identity.content_type != target.publication.content_type {
            return Err(SelectionError::Invalid);
        }
        let evidence_json = serde_json::to_string(evidence).map_err(|_| SelectionError::Invalid)?;
        let reasons_json =
            serde_json::to_string(&evaluation.reasons).map_err(|_| SelectionError::Invalid)?;
        if evidence_json.len() > 32768 || reasons_json.len() > 16384 {
            return Err(SelectionError::Invalid);
        }
        let current_time = now();
        if expires_at <= current_time || expires_at > current_time + 3600 {
            return Err(SelectionError::Invalid);
        }
        let policy = current_policy(connection, owner, &target.unit.id)
            .await?
            .filter(|policy| policy.id == policy_id)
            .ok_or(SelectionError::Changed)?;
        let row = sqlx::query(CONTEXT_SQL)
            .bind(&target.unit.id)
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(SelectionError::Changed)?;
        let current_target = context_row(&row).map_err(|_| SelectionError::Database)?;
        let current_target_fingerprint = target_fingerprint(&current_target)?;
        if current_target_fingerprint != target_fingerprint(target)? {
            return Err(SelectionError::Changed);
        }
        let computed = evaluate(
            &current_target,
            &Candidate {
                source: &identity.integration_id,
                content_type: Evidence::Unknown,
                evidence,
            },
        );
        if computed != *evaluation {
            return Err(SelectionError::Invalid);
        }
        let evidence_fingerprint = digest(&versioned_json("evidence-v1", &(evidence, &computed))?);
        if let Some(row) = sqlx::query("SELECT * FROM release_assessments WHERE owner = ? AND unit_id = ? AND policy_id = ? AND release_handle = ? AND integration_id = ? AND source_fingerprint = ? AND indexer_id = ? AND guid_digest = ? AND content_type = ? AND protocol = ? AND policy_fingerprint = ? AND target_fingerprint = ? AND evidence_fingerprint = ? AND expires_at > ? ORDER BY created_at DESC, id DESC LIMIT 1")
            .bind(owner).bind(&target.unit.id).bind(policy_id).bind(&identity.handle).bind(&identity.integration_id).bind(&identity.source_fingerprint).bind(i64::from(identity.indexer_id)).bind(&identity.guid_digest).bind(content_type_name(&identity.content_type)).bind(protocol_name(identity.protocol)).bind(&policy.fingerprint).bind(&current_target_fingerprint).bind(&evidence_fingerprint).bind(current_time)
            .fetch_optional(&mut *connection).await? {
            let assessment = assessment_row(row)?;
            return Ok(assessment);
        }
        let row = sqlx::query("INSERT INTO release_assessments (id, owner, unit_id, release_handle, integration_id, source_fingerprint, indexer_id, guid_digest, content_type, protocol, policy_id, policy_fingerprint, target_fingerprint, evidence_fingerprint, evidence_json, reasons_json, eligibility, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *")
            .bind(Uuid::new_v4().to_string()).bind(owner).bind(&target.unit.id).bind(&identity.handle).bind(&identity.integration_id).bind(&identity.source_fingerprint).bind(i64::from(identity.indexer_id)).bind(&identity.guid_digest).bind(content_type_name(&identity.content_type)).bind(protocol_name(identity.protocol)).bind(policy_id).bind(&policy.fingerprint).bind(&current_target_fingerprint).bind(&evidence_fingerprint).bind(evidence_json).bind(reasons_json).bind(eligibility_name(evaluation)).bind(expires_at)
            .fetch_one(&mut *connection).await?;
        let assessment = assessment_row(row)?;
        Ok(assessment)
    }

    pub(crate) async fn active_rejection_in(
        connection: &mut SqliteConnection,
        owner: &str,
        assessment: &Assessment,
    ) -> Result<Option<Decision>, SelectionError> {
        decisions::rejection_for(connection, owner, assessment).await
    }

    /// Validates the complete authorization chain while the caller holds its write transaction.
    pub(crate) async fn validate_selected_decision_in(
        connection: &mut SqliteConnection,
        owner: &str,
        decision_id: &str,
        unit_id: &str,
        release_handle: &str,
    ) -> Result<Assessment, SelectionError> {
        validate_scope(owner, unit_id)?;
        valid_id(decision_id)?;
        valid_id(release_handle)?;
        let decision = sqlx::query("SELECT * FROM release_decisions WHERE id = ? AND owner = ?")
            .bind(decision_id)
            .bind(owner)
            .fetch_optional(&mut *connection)
            .await?
            .map(decisions::decision_row)
            .transpose()?
            .ok_or(SelectionError::NotFound)?;
        if decision.action != DecisionAction::Selected {
            return Err(SelectionError::Invalid);
        }
        let assessment =
            decisions::load_assessment(connection, owner, &decision.assessment_id).await?;
        if assessment.unit_id != unit_id || assessment.identity.handle != release_handle {
            return Err(SelectionError::Changed);
        }
        decisions::validate_current(connection, owner, &assessment).await?;
        let source = sqlx::query(
            "SELECT kind, label, base_url, enabled, options FROM integrations WHERE id = ?",
        )
        .bind(&assessment.identity.integration_id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SelectionError::Changed)?;
        let kind: String = source.try_get("kind")?;
        if kind != "prowlarr" || !source.try_get::<bool, _>("enabled")? {
            return Err(SelectionError::Changed);
        }
        let options: ProwlarrOptions = serde_json::from_str(source.try_get("options")?)
            .map_err(|_| SelectionError::Database)?;
        let current_source = Integration {
            id: assessment.identity.integration_id.clone(),
            kind: IntegrationKind::Prowlarr,
            label: source.try_get("label")?,
            base_url: source.try_get("base_url")?,
            enabled: true,
            options: IntegrationOptions::Prowlarr(options),
            api_key_configured: false,
            username_configured: false,
            password_configured: false,
            credentials_configured: false,
        };
        if source_fingerprint(&current_source).map_err(|_| SelectionError::Database)?
            != assessment.identity.source_fingerprint
        {
            return Err(SelectionError::Changed);
        }
        let cached_evidence: Option<Option<String>> = sqlx::query_scalar("SELECT evidence_json FROM search_releases WHERE handle = ? AND owner = ? AND integration_id = ? AND source_fingerprint = ? AND indexer_id = ? AND guid_digest = ? AND content_type = ? AND protocol = ? AND expires_at > unixepoch()")
            .bind(release_handle)
            .bind(owner)
            .bind(&assessment.identity.integration_id)
            .bind(&assessment.identity.source_fingerprint)
            .bind(i64::from(assessment.identity.indexer_id))
            .bind(&assessment.identity.guid_digest)
            .bind(content_type_name(&assessment.identity.content_type))
            .bind(protocol_name(assessment.identity.protocol))
            .fetch_optional(&mut *connection)
            .await?;
        let cached_evidence = cached_evidence
            .flatten()
            .ok_or(SelectionError::Changed)
            .and_then(|json| {
                serde_json::from_str::<ReleaseEvidence>(&json).map_err(|_| SelectionError::Database)
            })?;
        if cached_evidence != assessment.evidence {
            return Err(SelectionError::Changed);
        }
        match assessment.evaluation.eligibility {
            Eligibility::Eligible if decision.acknowledged_assessment_id.is_some() => {
                return Err(SelectionError::Invalid);
            }
            Eligibility::Unknown
                if decision.acknowledged_assessment_id.as_deref()
                    != Some(assessment.id.as_str()) =>
            {
                return Err(SelectionError::AcknowledgementRequired);
            }
            Eligibility::Ineligible => return Err(SelectionError::Ineligible),
            _ => {}
        }
        if decisions::rejection_for(connection, owner, &assessment)
            .await?
            .is_some()
        {
            return Err(SelectionError::Rejected);
        }
        if decisions::superseded(connection, decision_id).await? {
            return Err(SelectionError::Superseded);
        }
        Ok(assessment)
    }
}

fn validate_identity(identity: &ReleaseIdentity) -> Result<(), SelectionError> {
    valid_id(&identity.handle)?;
    valid_id(&identity.integration_id)?;
    valid_digest(&identity.source_fingerprint)?;
    valid_digest(&identity.guid_digest)
}

fn valid_digest(value: &str) -> Result<(), SelectionError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(SelectionError::Invalid)
    }
}

fn versioned_json<T: Serialize>(version: &str, value: &T) -> Result<Vec<u8>, SelectionError> {
    serde_json::to_vec(&serde_json::json!([version, value])).map_err(|_| SelectionError::Invalid)
}

fn target_fingerprint(target: &UnitContext) -> Result<String, SelectionError> {
    Ok(digest(
        &serde_json::to_vec(&serde_json::json!([
            "target-v1",
            target.publication.id,
            target.publication.title,
            target.publication.run_label,
            target.publication.content_type,
            target.edition.id,
            target.edition.language,
            target.edition.region,
            target.edition.publisher,
            target.unit.id,
            target.unit.label,
            target.unit.kind,
            target.unit.date,
            target.unit.date_precision
        ]))
        .map_err(|_| SelectionError::Invalid)?,
    ))
}

fn content_type_name(value: &ContentType) -> &'static str {
    match value {
        ContentType::Comic => "comic",
        ContentType::Manga => "manga",
        ContentType::Magazine => "magazine",
    }
}
fn protocol_name(value: ReleaseProtocol) -> &'static str {
    match value {
        ReleaseProtocol::Usenet => "usenet",
        ReleaseProtocol::Torrent => "torrent",
    }
}
fn eligibility_name(value: &Evaluation) -> &'static str {
    match value.eligibility {
        super::matching::Eligibility::Eligible => "eligible",
        super::matching::Eligibility::Unknown => "unknown",
        super::matching::Eligibility::Ineligible => "ineligible",
    }
}

fn assessment_row(row: sqlx::sqlite::SqliteRow) -> Result<Assessment, SelectionError> {
    let protocol = match row.try_get::<String, _>("protocol")?.as_str() {
        "usenet" => ReleaseProtocol::Usenet,
        "torrent" => ReleaseProtocol::Torrent,
        _ => return Err(SelectionError::Database),
    };
    let eligibility = match row.try_get::<String, _>("eligibility")?.as_str() {
        "eligible" => super::matching::Eligibility::Eligible,
        "unknown" => super::matching::Eligibility::Unknown,
        "ineligible" => super::matching::Eligibility::Ineligible,
        _ => return Err(SelectionError::Database),
    };
    Ok(Assessment {
        id: row.try_get("id")?,
        unit_id: row.try_get("unit_id")?,
        policy_id: row.try_get("policy_id")?,
        policy_fingerprint: row.try_get("policy_fingerprint")?,
        target_fingerprint: row.try_get("target_fingerprint")?,
        evidence_fingerprint: row.try_get("evidence_fingerprint")?,
        identity: ReleaseIdentity {
            handle: row.try_get("release_handle")?,
            integration_id: row.try_get("integration_id")?,
            source_fingerprint: row.try_get("source_fingerprint")?,
            indexer_id: u32::try_from(row.try_get::<i64, _>("indexer_id")?)
                .map_err(|_| SelectionError::Database)?,
            guid_digest: row.try_get("guid_digest")?,
            content_type: ContentType::parse(row.try_get("content_type")?)
                .map_err(|_| SelectionError::Database)?,
            protocol,
        },
        evidence: serde_json::from_str(row.try_get("evidence_json")?)
            .map_err(|_| SelectionError::Database)?,
        evaluation: Evaluation {
            eligibility,
            reasons: serde_json::from_str(row.try_get("reasons_json")?)
                .map_err(|_| SelectionError::Database)?,
        },
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
    })
}

fn validate_scope(owner: &str, unit_id: &str) -> Result<(), SelectionError> {
    validate_label(owner).map_err(|_| SelectionError::Invalid)?;
    valid_id(unit_id)
}

fn valid_id(id: &str) -> Result<(), SelectionError> {
    if Uuid::parse_str(id).is_ok_and(|parsed| !parsed.is_nil() && parsed.to_string() == id) {
        Ok(())
    } else {
        Err(SelectionError::Invalid)
    }
}

fn validate_draft(draft: &PolicyDraft) -> Result<(), SelectionError> {
    if draft.format_order.len() > 3 || draft.source_priority.len() > 64 {
        return Err(SelectionError::Invalid);
    }
    for (index, format) in draft.format_order.iter().enumerate() {
        if draft.format_order[..index].contains(format) {
            return Err(SelectionError::Invalid);
        }
    }
    for (index, source) in draft.source_priority.iter().enumerate() {
        valid_id(source)?;
        if draft.source_priority[..index].contains(source) {
            return Err(SelectionError::Invalid);
        }
    }
    Ok(())
}

async fn require_unit(
    connection: &mut SqliteConnection,
    unit_id: &str,
) -> Result<(), SelectionError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM units WHERE id = ?)")
        .bind(unit_id)
        .fetch_one(connection)
        .await?;
    if exists {
        Ok(())
    } else {
        Err(SelectionError::NotFound)
    }
}

async fn current_policy(
    connection: &mut SqliteConnection,
    owner: &str,
    unit_id: &str,
) -> Result<Option<SelectionPolicy>, SelectionError> {
    sqlx::query("SELECT * FROM release_selection_policies WHERE owner = ? AND unit_id = ? ORDER BY revision DESC LIMIT 1")
        .bind(owner)
        .bind(unit_id)
        .fetch_optional(connection)
        .await?
        .map(policy_row)
        .transpose()
}

async fn insert_policy(
    connection: &mut SqliteConnection,
    owner: &str,
    unit_id: &str,
    revision: i64,
    draft: PolicyDraft,
) -> Result<SelectionPolicy, SelectionError> {
    let format_order =
        serde_json::to_string(&draft.format_order).map_err(|_| SelectionError::Invalid)?;
    let source_priority =
        serde_json::to_string(&draft.source_priority).map_err(|_| SelectionError::Invalid)?;
    let canonical = serde_json::to_vec(&serde_json::json!([
        "policy-v1",
        "review_only",
        draft.format_order,
        draft.source_priority
    ]))
    .map_err(|_| SelectionError::Invalid)?;
    let row = sqlx::query("INSERT INTO release_selection_policies (id, owner, unit_id, revision, mode, format_order, source_priority, fingerprint) VALUES (?, ?, ?, ?, 'review_only', ?, ?, ?) RETURNING *")
        .bind(Uuid::new_v4().to_string())
        .bind(owner)
        .bind(unit_id)
        .bind(revision)
        .bind(format_order)
        .bind(source_priority)
        .bind(digest(&canonical))
        .fetch_one(connection)
        .await?;
    policy_row(row)
}

fn policy_row(row: sqlx::sqlite::SqliteRow) -> Result<SelectionPolicy, SelectionError> {
    Ok(SelectionPolicy {
        id: row.try_get("id")?,
        unit_id: row.try_get("unit_id")?,
        revision: row.try_get("revision")?,
        preferences: PolicyDraft {
            format_order: serde_json::from_str(row.try_get("format_order")?)
                .map_err(|_| SelectionError::Database)?,
            source_priority: serde_json::from_str(row.try_get("source_priority")?)
                .map_err(|_| SelectionError::Database)?,
        },
        fingerprint: row.try_get("fingerprint")?,
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
#[path = "selection_test.rs"]
mod tests;

#[cfg(test)]
#[path = "selection_decision_test.rs"]
mod decision_tests;
