use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};
use std::sync::Arc;
use tokio::sync::Mutex;
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
    pub raw_volumes: Vec<String>,
    pub date_warnings: Vec<ArchiveUnitMatch>,
    pub raw_dates: Vec<String>,
    pub combined: bool,
    pub identifier: String,
    pub source_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchiveUnitMatch {
    pub id: String,
    pub label: String,
    pub edition_id: String,
    pub publication_id: String,
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
        existing_unit: ArchiveUnitMatch,
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
        let mut proposal = proposal(&request.edition_id, item.magazine);
        let mut connection = self.store.reader().acquire().await?;
        check_catalog(&mut connection, &mut proposal).await?;
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
        let lock = {
            let mut locks = self.archive_accepts.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks
                .get(&request.idempotency_key)
                .and_then(std::sync::Weak::upgrade)
            {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(request.idempotency_key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().await;
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
        let mut source_proposal = proposal(&request.edition_id, item.magazine);
        if source_proposal.combined && !matches!(request.kind, UnitKind::Issue | UnitKind::Combined)
        {
            return Err(CatalogError::Invalid(
                "Combined Archive issues require kind issue or combined".into(),
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
        check_catalog(&mut tx, &mut reviewed).await?;
        source_proposal.date_warnings = reviewed.date_warnings.clone();
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
        sqlx::query("INSERT INTO archive_unit_acceptances (owner, unit_id, idempotency_key, request_fingerprint, response_json) VALUES (?, ?, ?, ?, ?)")
            .bind(owner).bind(&accepted.unit.id).bind(&request.idempotency_key).bind(fingerprint)
            .bind(serde_json::to_string(&accepted).map_err(|_| SearchError::Database)?)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(accepted)
    }

    async fn archive_edition(&self, edition_id: &str) -> Result<(), ArchiveProposalError> {
        let mut connection = self.store.reader().acquire().await?;
        magazine_edition(&mut connection, edition_id).await?;
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
        magazine
            .issues
            .iter()
            .map(|value| value.trim())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let volume = magazine
        .volumes
        .iter()
        .map(|value| value.trim())
        .collect::<Vec<_>>()
        .join(" / ");
    let label = if volume.is_empty() {
        label
    } else if magazine.issues.is_empty() {
        format!("Vol {volume}")
    } else {
        format!("Vol {volume} No {label}")
    };
    let combined = magazine.issues.iter().any(|value| combined_label(value))
        || (magazine.issues.len() == 2 && combined_label(&magazine.issues.join(" / ")));
    ArchiveUnitProposal {
        edition_id: edition_id.into(),
        label,
        kind: if combined {
            UnitKind::Combined
        } else {
            UnitKind::Issue
        },
        date_precision: date.as_deref().and_then(|value| validate_date(value).ok()),
        date,
        raw_labels: magazine.issues,
        raw_volumes: magazine.volumes,
        date_warnings: Vec::new(),
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
    let row = sqlx::query("SELECT owner, request_fingerprint, response_json FROM archive_unit_acceptances WHERE idempotency_key = ?")
        .bind(key).fetch_optional(&mut **tx).await?;
    if let Some(row) = row {
        if row.get::<String, _>("owner") != owner
            || row.get::<String, _>("request_fingerprint") != fingerprint
        {
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

async fn magazine_edition(
    connection: &mut sqlx::SqliteConnection,
    edition_id: &str,
) -> Result<(), ArchiveProposalError> {
    let kind: Option<String> = sqlx::query_scalar("SELECT p.content_type FROM editions e JOIN publications p ON p.id = e.publication_id WHERE e.id = ?")
        .bind(edition_id).fetch_optional(&mut *connection).await?;
    match kind.as_deref() {
        Some("magazine") => Ok(()),
        Some(_) => {
            Err(CatalogError::Invalid("Archive proposals require a magazine edition".into()).into())
        }
        None => Err(CatalogError::NotFound.into()),
    }
}

async fn check_catalog(
    connection: &mut sqlx::SqliteConnection,
    proposal: &mut ArchiveUnitProposal,
) -> Result<(), ArchiveProposalError> {
    magazine_edition(connection, &proposal.edition_id).await?;
    let linked = sqlx::query("SELECT u.id, u.label, u.edition_id, e.publication_id FROM provider_links l JOIN units u ON u.id = l.unit_id JOIN editions e ON e.id = u.edition_id WHERE l.provider = 'internet_archive' AND l.external_id = ?")
        .bind(&proposal.identifier).fetch_optional(&mut *connection).await?;
    let rows = sqlx::query("SELECT u.id, u.label, u.edition_id, e.publication_id, u.date FROM units u JOIN editions e ON e.id = u.edition_id WHERE u.edition_id = ? ORDER BY u.id")
        .bind(&proposal.edition_id).fetch_all(&mut *connection).await?;
    let normalized = normalized_label(&proposal.label);
    let duplicate = linked.as_ref().or_else(|| {
        rows.iter()
            .find(|row| normalized_label(&row.get::<String, _>("label")) == normalized)
    });
    proposal.date_warnings = rows
        .iter()
        .filter(|row| {
            proposal
                .date
                .as_deref()
                .zip(row.get::<Option<String>, _>("date").as_deref())
                .is_some_and(|(date, existing)| dates_overlap(date, existing))
        })
        .map(unit_match)
        .collect();
    if let Some(row) = duplicate {
        return Err(ArchiveProposalError::Duplicate {
            existing_unit: unit_match(row),
            proposal: Box::new(proposal.clone()),
        });
    }
    Ok(())
}

fn unit_match(row: &sqlx::sqlite::SqliteRow) -> ArchiveUnitMatch {
    ArchiveUnitMatch {
        id: row.get("id"),
        label: row.get("label"),
        edition_id: row.get("edition_id"),
        publication_id: row.get("publication_id"),
    }
}

fn dates_overlap(left: &str, right: &str) -> bool {
    let length = if left.len() >= 7 && right.len() >= 7 {
        7
    } else {
        4
    };
    left.get(..length)
        .is_some_and(|prefix| right.get(..length) == Some(prefix))
}

fn combined_label(raw: &str) -> bool {
    let value = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if validate_date(&value).is_ok() {
        return false;
    }
    for separator in ["-", "–", "—", "/", " and ", "&"] {
        if let Some((left, right)) = value.split_once(separator) {
            let (left, right) = (left.trim(), right.trim());
            let number =
                |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            if number(left) && number(right) {
                return !(left.len() == 4
                    && right.len() == 2
                    && validate_date(&format!("{left}-{right}")).is_ok());
            }
            let month = |part: &str| {
                matches!(
                    part,
                    "jan"
                        | "january"
                        | "feb"
                        | "february"
                        | "mar"
                        | "march"
                        | "apr"
                        | "april"
                        | "may"
                        | "jun"
                        | "june"
                        | "jul"
                        | "july"
                        | "aug"
                        | "august"
                        | "sep"
                        | "sept"
                        | "september"
                        | "oct"
                        | "october"
                        | "nov"
                        | "november"
                        | "dec"
                        | "december"
                )
            };
            if month(left) && month(right) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_labels_require_real_numeric_or_month_pairs() {
        for (label, expected) in [
            ("3-4", true),
            ("3–4", true),
            ("3—4", true),
            ("3/4", true),
            ("3 and 4", true),
            ("3 & 4", true),
            ("Jan/Feb", true),
            ("Jan-Feb", true),
            ("January–February", true),
            ("Vol. 3, No. 4", false),
            ("12-A", false),
            ("2024-03", false),
            ("3,4", false),
            (",", false),
            ("3", false),
        ] {
            assert_eq!(combined_label(label), expected, "{label}");
        }
    }
}
