use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};
use uuid::Uuid;

use super::{Search, SearchError};
use crate::{
    catalog::{CatalogError, DatePrecision, Unit, UnitKind, validate_date, validate_required},
    providers::internet_archive::ArchiveMagazine,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveProposalRequest {
    pub integration_id: String,
    pub identifier: String,
    pub edition_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchiveUnitProposal {
    pub edition_id: String,
    pub label: String,
    pub kind: UnitKind,
    pub date: Option<String>,
    pub date_precision: Option<DatePrecision>,
    pub raw_labels: Vec<String>,
    pub raw_dates: Vec<String>,
    pub combined: bool,
    pub identifier: String,
    pub source_url: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveAcceptRequest {
    pub integration_id: String,
    pub identifier: String,
    pub edition_id: String,
    pub label: String,
    pub kind: UnitKind,
    pub date: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ArchiveUnitAcceptance {
    pub unit: Unit,
    pub proposal: ArchiveUnitProposal,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveProposalError {
    #[error(transparent)]
    Search(#[from] SearchError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("archive unit already exists")]
    Duplicate {
        existing_unit_id: String,
        proposal: Box<ArchiveUnitProposal>,
    },
    #[error("idempotency key was already used with a different request")]
    IdempotencyConflict,
    #[error("archive proposal database operation failed")]
    Database(#[from] sqlx::Error),
}

impl Search {
    pub async fn archive_unit_proposal(
        &self,
        request: &ArchiveProposalRequest,
    ) -> Result<ArchiveUnitProposal, ArchiveProposalError> {
        self.archive_edition(&request.edition_id).await?;
        let item = self
            .archive_item(&request.integration_id, &request.identifier)
            .await?;
        let proposal = proposal(&request.edition_id, item.magazine);
        let mut connection = self.store.reader().acquire().await?;
        check_duplicate(&mut connection, &proposal).await?;
        Ok(proposal)
    }

    pub async fn accept_archive_unit(
        &self,
        owner: &str,
        request: ArchiveAcceptRequest,
    ) -> Result<ArchiveUnitAcceptance, ArchiveProposalError> {
        validate_required(&request.idempotency_key, "idempotency_key")?;
        if request.idempotency_key.len() > 128 {
            return Err(CatalogError::Invalid("idempotency_key is too long".into()).into());
        }
        let fingerprint = super::service::digest(
            &serde_json::to_vec(&request).map_err(|_| SearchError::Invalid)?,
        );
        {
            let mut tx = self.store.begin_write().await?;
            if let Some(accepted) =
                replay(&mut tx, owner, &request.idempotency_key, &fingerprint).await?
            {
                return Ok(accepted);
            }
        }
        validate_required(&request.label, "label")?;
        let precision = request.date.as_deref().map(validate_date).transpose()?;
        self.archive_edition(&request.edition_id).await?;
        let item = self
            .archive_item(&request.integration_id, &request.identifier)
            .await?;
        let source_proposal = proposal(&request.edition_id, item.magazine);
        if source_proposal.combined && request.kind != UnitKind::Issue {
            return Err(CatalogError::Invalid(
                "Combined Archive issues must remain one issue".into(),
            )
            .into());
        }
        let mut reviewed = source_proposal.clone();
        reviewed.label = request.label;
        reviewed.kind = request.kind;
        reviewed.date = request.date;
        reviewed.date_precision = precision;
        let mut tx = self.store.begin_write().await?;
        if let Some(accepted) =
            replay(&mut tx, owner, &request.idempotency_key, &fingerprint).await?
        {
            return Ok(accepted);
        }
        check_duplicate(&mut tx, &source_proposal).await?;
        check_duplicate(&mut tx, &reviewed).await?;
        let unit = Unit {
            id: Uuid::new_v4().to_string(),
            edition_id: reviewed.edition_id.clone(),
            label: reviewed.label,
            kind: reviewed.kind,
            sort_key: None,
            date: reviewed.date,
            date_precision: reviewed.date_precision,
            label_locked: false,
            kind_locked: false,
            sort_key_locked: false,
            date_locked: false,
        };
        sqlx::query("INSERT INTO units (id, edition_id, label, kind, date, date_precision) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&unit.id).bind(&unit.edition_id).bind(&unit.label).bind(unit.kind.as_str())
            .bind(&unit.date).bind(unit.date_precision.as_ref().map(DatePrecision::as_str))
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO provider_links (id, provider, external_id, unit_id) VALUES (?, 'internet_archive', ?, ?)")
            .bind(Uuid::new_v4().to_string()).bind(&source_proposal.identifier).bind(&unit.id)
            .execute(&mut *tx).await?;
        let accepted = ArchiveUnitAcceptance {
            unit,
            proposal: source_proposal,
        };
        sqlx::query("INSERT INTO archive_unit_acceptances (owner, idempotency_key, request_fingerprint, response_json) VALUES (?, ?, ?, ?)")
            .bind(owner).bind(&request.idempotency_key).bind(fingerprint)
            .bind(serde_json::to_string(&accepted).map_err(|_| SearchError::Database)?)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(accepted)
    }

    async fn archive_edition(&self, edition_id: &str) -> Result<(), ArchiveProposalError> {
        if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM editions WHERE id = ?")
            .bind(edition_id)
            .fetch_one(self.store.reader())
            .await?
            == 0
        {
            return Err(CatalogError::NotFound.into());
        }
        Ok(())
    }
}

fn proposal(edition_id: &str, magazine: ArchiveMagazine) -> ArchiveUnitProposal {
    // Ambiguous dates stay raw; a single exact calendar date retains its source precision.
    let date = if magazine.dates.len() == 1 && validate_date(magazine.dates[0].trim()).is_ok() {
        Some(magazine.dates[0].trim().to_string())
    } else {
        None
    };
    let label = if magazine.issues.is_empty() {
        date.clone().unwrap_or_else(|| magazine.title.clone())
    } else {
        magazine.issues.join(" / ")
    };
    let combined = magazine.issues.len() > 1
        || magazine
            .issues
            .iter()
            .any(|value| value.contains(['-', '/', '+', '&', ',']));
    ArchiveUnitProposal {
        edition_id: edition_id.into(),
        label,
        kind: UnitKind::Issue,
        date_precision: date.as_deref().and_then(|value| validate_date(value).ok()),
        date,
        raw_labels: magazine.issues,
        raw_dates: magazine.dates,
        combined,
        source_url: format!("https://archive.org/details/{}", magazine.identifier),
        identifier: magazine.identifier,
    }
}

async fn replay(
    tx: &mut Transaction<'_, Sqlite>,
    owner: &str,
    key: &str,
    fingerprint: &str,
) -> Result<Option<ArchiveUnitAcceptance>, ArchiveProposalError> {
    let row = sqlx::query("SELECT request_fingerprint, response_json FROM archive_unit_acceptances WHERE owner = ? AND idempotency_key = ?")
        .bind(owner).bind(key).fetch_optional(&mut **tx).await?;
    if let Some(row) = row {
        if row.get::<String, _>("request_fingerprint") != fingerprint {
            return Err(ArchiveProposalError::IdempotencyConflict);
        }
        return serde_json::from_str(&row.get::<String, _>("response_json"))
            .map(Some)
            .map_err(|_| SearchError::Database.into());
    }
    Ok(None)
}

fn normalized_label(label: &str) -> String {
    let label = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if label.bytes().all(|byte| byte.is_ascii_digit()) && !label.is_empty() {
        let number = label.trim_start_matches('0');
        if number.is_empty() {
            "0".into()
        } else {
            number.into()
        }
    } else {
        label
    }
}

async fn check_duplicate(
    tx: &mut sqlx::SqliteConnection,
    proposal: &ArchiveUnitProposal,
) -> Result<(), ArchiveProposalError> {
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM editions WHERE id = ?")
        .bind(&proposal.edition_id)
        .fetch_one(&mut *tx)
        .await?
        == 0
    {
        return Err(CatalogError::NotFound.into());
    }
    let linked: Option<String> = sqlx::query_scalar("SELECT unit_id FROM provider_links WHERE provider = 'internet_archive' AND external_id = ? AND unit_id IS NOT NULL")
        .bind(&proposal.identifier).fetch_optional(&mut *tx).await?;
    let rows = sqlx::query("SELECT u.id, u.label, u.date, u.date_precision, p.content_type FROM units u JOIN editions e ON e.id = u.edition_id JOIN publications p ON p.id = e.publication_id WHERE u.edition_id = ? ORDER BY u.id")
        .bind(&proposal.edition_id).fetch_all(&mut *tx).await?;
    let duplicate = linked.or_else(|| {
        rows.iter()
            .find(|row| {
                normalized_label(&row.get::<String, _>("label"))
                    == normalized_label(&proposal.label)
                    || (row.get::<String, _>("content_type") == "magazine"
                        && proposal.date.is_some()
                        && row.get::<Option<String>, _>("date") == proposal.date
                        && row.get::<Option<String>, _>("date_precision").as_deref()
                            == proposal.date_precision.as_ref().map(DatePrecision::as_str))
            })
            .map(|row| row.get("id"))
    });
    if let Some(existing_unit_id) = duplicate {
        return Err(ArchiveProposalError::Duplicate {
            existing_unit_id,
            proposal: Box::new(proposal.clone()),
        });
    }
    Ok(())
}
