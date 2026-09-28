use super::*;
use crate::search::matching::Eligibility;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAction {
    Selected,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewDecision {
    pub assessment_id: String,
    pub action: DecisionAction,
    pub acknowledged_assessment_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Decision {
    pub id: String,
    pub assessment_id: String,
    pub action: DecisionAction,
    pub acknowledged_assessment_id: Option<String>,
    pub reason: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Revocation {
    pub id: String,
    pub rejection_decision_id: String,
    pub created_at: i64,
}

impl SelectionRepository {
    pub async fn decide(
        &self,
        owner: &str,
        key: &str,
        request: NewDecision,
    ) -> Result<Decision, SelectionError> {
        validate_actor_key(owner, key)?;
        valid_id(&request.assessment_id)?;
        if let Some(id) = &request.acknowledged_assessment_id {
            valid_id(id)?;
        }
        if request.reason.as_ref().is_some_and(|reason| {
            reason.is_empty() || reason.len() > 512 || reason.chars().any(char::is_control)
        }) {
            return Err(SelectionError::Invalid);
        }
        let fingerprint = digest(&versioned_json("decision-v1", &request)?);
        let mut tx = self.store.begin_write().await?;
        if let Some(row) =
            sqlx::query("SELECT * FROM release_decisions WHERE owner = ? AND idempotency_key = ?")
                .bind(owner)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?
        {
            if row.try_get::<String, _>("request_fingerprint")? != fingerprint {
                return Err(SelectionError::Conflict);
            }
            let decision = decision_row(row)?;
            tx.commit().await?;
            return Ok(decision);
        }
        let assessment = load_assessment(&mut tx, owner, &request.assessment_id).await?;
        validate_current(&mut tx, owner, &assessment).await?;
        match request.action {
            DecisionAction::Selected => {
                match assessment.evaluation.eligibility {
                    Eligibility::Ineligible => return Err(SelectionError::Ineligible),
                    Eligibility::Unknown => {
                        if request.acknowledged_assessment_id.as_deref()
                            != Some(assessment.id.as_str())
                        {
                            return Err(SelectionError::AcknowledgementRequired);
                        }
                    }
                    Eligibility::Eligible => {
                        if request.acknowledged_assessment_id.is_some() {
                            return Err(SelectionError::Invalid);
                        }
                    }
                }
                if rejection_for(&mut tx, owner, &assessment).await?.is_some() {
                    return Err(SelectionError::Rejected);
                }
            }
            DecisionAction::Rejected if request.acknowledged_assessment_id.is_some() => {
                return Err(SelectionError::Invalid);
            }
            DecisionAction::Rejected => {
                if rejection_for(&mut tx, owner, &assessment).await?.is_some() {
                    return Err(SelectionError::Conflict);
                }
            }
        }
        let action = match request.action {
            DecisionAction::Selected => "selected",
            DecisionAction::Rejected => "rejected",
        };
        let row = sqlx::query("INSERT INTO release_decisions (id, owner, assessment_id, action, reason, acknowledged_assessment_id, idempotency_key, request_fingerprint) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *")
            .bind(Uuid::new_v4().to_string()).bind(owner).bind(&request.assessment_id).bind(action).bind(&request.reason).bind(&request.acknowledged_assessment_id).bind(key).bind(fingerprint)
            .fetch_one(&mut *tx).await?;
        let decision = decision_row(row)?;
        if decision.action == DecisionAction::Rejected {
            supersede_selections(&mut tx, owner, &assessment, &decision.id).await?;
        }
        tx.commit().await?;
        Ok(decision)
    }

    pub async fn active_rejection(
        &self,
        owner: &str,
        assessment_id: &str,
    ) -> Result<Option<Decision>, SelectionError> {
        validate_label(owner).map_err(|_| SelectionError::Invalid)?;
        valid_id(assessment_id)?;
        let mut tx = self.store.begin_write().await?;
        let assessment = load_assessment(&mut tx, owner, assessment_id).await?;
        let rejection = rejection_for(&mut tx, owner, &assessment).await?;
        tx.commit().await?;
        Ok(rejection)
    }

    pub async fn revoke_rejection(
        &self,
        owner: &str,
        key: &str,
        rejection_id: &str,
    ) -> Result<Revocation, SelectionError> {
        validate_actor_key(owner, key)?;
        valid_id(rejection_id)?;
        let fingerprint = digest(&versioned_json("revocation-v1", &rejection_id)?);
        let mut tx = self.store.begin_write().await?;
        if let Some(row) = sqlx::query(
            "SELECT * FROM release_rejection_revocations WHERE owner = ? AND idempotency_key = ?",
        )
        .bind(owner)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.try_get::<String, _>("request_fingerprint")? != fingerprint {
                return Err(SelectionError::Conflict);
            }
            let revocation = revocation_row(row)?;
            tx.commit().await?;
            return Ok(revocation);
        }
        let row = sqlx::query("SELECT action FROM release_decisions WHERE owner = ? AND id = ?")
            .bind(owner)
            .bind(rejection_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(SelectionError::NotFound)?;
        if row.try_get::<&str, _>("action")? != "rejected" {
            return Err(SelectionError::Invalid);
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_rejection_revocations WHERE rejection_decision_id = ?)")
            .bind(rejection_id).fetch_one(&mut *tx).await?;
        if exists {
            return Err(SelectionError::Conflict);
        }
        let row = sqlx::query("INSERT INTO release_rejection_revocations (id, owner, rejection_decision_id, idempotency_key, request_fingerprint) VALUES (?, ?, ?, ?, ?) RETURNING *")
            .bind(Uuid::new_v4().to_string()).bind(owner).bind(rejection_id).bind(key).bind(fingerprint).fetch_one(&mut *tx).await?;
        let revocation = revocation_row(row)?;
        tx.commit().await?;
        Ok(revocation)
    }

    pub async fn cleanup_expired(&self, limit: u32) -> Result<u64, SelectionError> {
        if !(1..=1000).contains(&limit) {
            return Err(SelectionError::Invalid);
        }
        let mut tx = self.store.begin_write().await?;
        let result = sqlx::query("DELETE FROM release_assessments WHERE id IN (SELECT a.id FROM release_assessments a WHERE expires_at <= unixepoch() AND NOT EXISTS (SELECT 1 FROM release_decisions d WHERE d.assessment_id = a.id OR d.acknowledged_assessment_id = a.id) ORDER BY expires_at, id LIMIT ?)")
            .bind(limit).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }
}

fn validate_actor_key(owner: &str, key: &str) -> Result<(), SelectionError> {
    validate_label(owner).map_err(|_| SelectionError::Invalid)?;
    validate_label(key).map_err(|_| SelectionError::Invalid)
}

pub(super) async fn load_assessment(
    connection: &mut SqliteConnection,
    owner: &str,
    id: &str,
) -> Result<Assessment, SelectionError> {
    let row = sqlx::query("SELECT * FROM release_assessments WHERE owner = ? AND id = ?")
        .bind(owner)
        .bind(id)
        .fetch_optional(connection)
        .await?
        .ok_or(SelectionError::NotFound)?;
    assessment_row(row)
}

pub(super) async fn validate_current(
    connection: &mut SqliteConnection,
    owner: &str,
    assessment: &Assessment,
) -> Result<(), SelectionError> {
    if assessment.expires_at <= now() {
        return Err(SelectionError::Expired);
    }
    let policy = current_policy(connection, owner, &assessment.unit_id)
        .await?
        .ok_or(SelectionError::Changed)?;
    if policy.id != assessment.policy_id || policy.fingerprint != assessment.policy_fingerprint {
        return Err(SelectionError::Changed);
    }
    let row = sqlx::query(CONTEXT_SQL)
        .bind(&assessment.unit_id)
        .fetch_optional(connection)
        .await?
        .ok_or(SelectionError::Changed)?;
    let target = context_row(&row).map_err(|_| SelectionError::Database)?;
    if target_fingerprint(&target)? != assessment.target_fingerprint {
        return Err(SelectionError::Changed);
    }
    Ok(())
}

pub(super) async fn rejection_for(
    connection: &mut SqliteConnection,
    owner: &str,
    assessment: &Assessment,
) -> Result<Option<Decision>, SelectionError> {
    sqlx::query("SELECT d.* FROM release_decisions d JOIN release_assessments a ON a.id = d.assessment_id WHERE d.owner = ? AND a.owner = d.owner AND a.unit_id = ? AND a.policy_fingerprint = ? AND a.integration_id = ? AND a.indexer_id = ? AND a.guid_digest = ? AND a.content_type = ? AND a.protocol = ? AND d.action = 'rejected' AND NOT EXISTS (SELECT 1 FROM release_rejection_revocations r WHERE r.rejection_decision_id = d.id) ORDER BY d.created_at DESC, d.id DESC LIMIT 1")
        .bind(owner).bind(&assessment.unit_id).bind(&assessment.policy_fingerprint).bind(&assessment.identity.integration_id).bind(i64::from(assessment.identity.indexer_id)).bind(&assessment.identity.guid_digest).bind(content_type_name(&assessment.identity.content_type)).bind(protocol_name(assessment.identity.protocol))
        .fetch_optional(connection).await?.map(decision_row).transpose()
}

/// Every selection in the rejection scope is permanently superseded, even after a revocation.
async fn supersede_selections(
    connection: &mut SqliteConnection,
    owner: &str,
    assessment: &Assessment,
    rejection_id: &str,
) -> Result<(), SelectionError> {
    sqlx::query("UPDATE release_decisions SET superseded_by = ? WHERE owner = ? AND action = 'selected' AND superseded_by IS NULL AND assessment_id IN (SELECT id FROM release_assessments WHERE owner = ? AND unit_id = ? AND policy_fingerprint = ? AND integration_id = ? AND indexer_id = ? AND guid_digest = ? AND content_type = ? AND protocol = ?)")
        .bind(rejection_id).bind(owner).bind(owner).bind(&assessment.unit_id).bind(&assessment.policy_fingerprint).bind(&assessment.identity.integration_id).bind(i64::from(assessment.identity.indexer_id)).bind(&assessment.identity.guid_digest).bind(content_type_name(&assessment.identity.content_type)).bind(protocol_name(assessment.identity.protocol))
        .execute(connection).await?;
    Ok(())
}

pub(super) async fn superseded(
    connection: &mut SqliteConnection,
    decision_id: &str,
) -> Result<bool, SelectionError> {
    Ok(
        sqlx::query_scalar("SELECT superseded_by IS NOT NULL FROM release_decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_optional(connection)
            .await?
            .unwrap_or(true),
    )
}

pub(super) fn decision_row(row: sqlx::sqlite::SqliteRow) -> Result<Decision, SelectionError> {
    let action = match row.try_get::<&str, _>("action")? {
        "selected" => DecisionAction::Selected,
        "rejected" => DecisionAction::Rejected,
        _ => return Err(SelectionError::Database),
    };
    Ok(Decision {
        id: row.try_get("id")?,
        assessment_id: row.try_get("assessment_id")?,
        action,
        acknowledged_assessment_id: row.try_get("acknowledged_assessment_id")?,
        reason: row.try_get("reason")?,
        created_at: row.try_get("created_at")?,
    })
}

fn revocation_row(row: sqlx::sqlite::SqliteRow) -> Result<Revocation, SelectionError> {
    Ok(Revocation {
        id: row.try_get("id")?,
        rejection_decision_id: row.try_get("rejection_decision_id")?,
        created_at: row.try_get("created_at")?,
    })
}
