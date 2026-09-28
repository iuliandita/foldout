//! Internal service only: the caller must authorize both registered roots and unit coverage.
use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use super::finalize::{self, Identity, IntentIdentity};
use crate::{
    reader::{archive::ArchiveDecoder, pdf::PdfDecoder},
    store::sqlite::SqliteStore,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ImportPolicy {
    Copy,
    Hardlink { fallback_to_copy: bool },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InternalImportRequest {
    pub source_root: String,
    pub source_relative: String,
    pub destination_root: String,
    pub destination_relative: String,
    pub unit_id: String,
    pub policy: ImportPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPhase {
    Planned,
    Staged,
    Verified,
    Finalized,
    Cataloged,
    CleanupPending,
    Done,
}

impl ImportPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Staged => "staged",
            Self::Verified => "verified",
            Self::Finalized => "finalized",
            Self::Cataloged => "cataloged",
            Self::CleanupPending => "cleanup_pending",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportOperation {
    pub id: String,
    pub phase: ImportPhase,
    pub effective_policy: Option<String>,
    pub validation: Option<String>,
    pub library_file_id: Option<String>,
    pub last_error: Option<String>,
    #[serde(skip)]
    pub(crate) request: InternalImportRequest,
    #[serde(skip)]
    pub(crate) identity: IntentIdentity,
    #[serde(skip)]
    pub(crate) staged: Option<Identity>,
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("import operation or registered root was not found")]
    NotFound,
    #[error("invalid import request or unsafe path")]
    UnsafePath,
    #[error("operation ID or destination conflicts with existing data")]
    Conflict,
    #[error("file identity or content changed; review is required")]
    Changed,
    #[error("hardlink crosses filesystems and copy fallback is disabled")]
    CrossFilesystem,
    #[error("another importer owns the destination root; retry later")]
    Busy,
    #[error("file format validation failed")]
    InvalidFormat,
    #[error("import journal is inconsistent")]
    InvalidJournal,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("import filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("import worker failed")]
    Worker,
}

#[derive(Clone)]
pub struct ImportService {
    pub(crate) store: SqliteStore,
    pub(crate) decoder: ArchiveDecoder,
    pub(crate) pdf_decoder: PdfDecoder,
    pub(crate) slots: Arc<tokio::sync::Semaphore>,
}

impl ImportService {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            decoder: ArchiveDecoder::new(),
            pdf_decoder: PdfDecoder::new(),
            slots: crate::library::roots::hash_slots(),
        }
    }

    /// Commit immutable intent and reserve exactly one destination before any filesystem write.
    /// Persist the UUID at the calling job before invoking this method; retries reuse that UUID.
    pub async fn plan(
        &self,
        operation_id: &str,
        request: InternalImportRequest,
    ) -> Result<ImportOperation, ImportError> {
        let id = Uuid::parse_str(operation_id)
            .map_err(|_| ImportError::UnsafePath)?
            .to_string();
        if id != operation_id {
            return Err(ImportError::UnsafePath);
        }
        finalize::relative(&request.source_relative)?;
        finalize::relative(&request.destination_relative)?;
        if let Some(existing) = self.optional(&id).await? {
            return if existing.request == request {
                Ok(existing)
            } else {
                Err(ImportError::Conflict)
            };
        }
        let source = self.root_path(&request.source_root).await?;
        let destination = self.root_path(&request.destination_root).await?;
        let req = request.clone();
        let identity = self
            .blocking(move || finalize::inspect_intent(source, destination, &req))
            .await?;
        let destination_path = identity.destination_path(&request);
        let request_json =
            serde_json::to_string(&request).map_err(|_| ImportError::InvalidJournal)?;
        let identity_json =
            serde_json::to_string(&identity).map_err(|_| ImportError::InvalidJournal)?;
        let mut tx = self.store.begin_write().await?;
        let result = sqlx::query("INSERT INTO import_operations (id, source_root, destination_root, destination_relative, destination_path, unit_id, request_json, identity_json, phase) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'planned')")
            .bind(&id).bind(&request.source_root).bind(&request.destination_root)
            .bind(&request.destination_relative).bind(destination_path.to_string_lossy().as_ref())
            .bind(&request.unit_id).bind(request_json).bind(identity_json).execute(&mut *tx).await;
        match result {
            Ok(_) => tx.commit().await?,
            Err(error)
                if error
                    .as_database_error()
                    .is_some_and(|e| e.is_unique_violation()) =>
            {
                tx.rollback().await?;
                let existing = self.optional(&id).await?;
                return match existing {
                    Some(existing) if existing.request == request => Ok(existing),
                    _ => Err(ImportError::Conflict),
                };
            }
            Err(error) => return Err(error.into()),
        }
        self.get(&id).await
    }

    pub async fn get(&self, id: &str) -> Result<ImportOperation, ImportError> {
        self.optional(id).await?.ok_or(ImportError::NotFound)
    }

    async fn optional(&self, id: &str) -> Result<Option<ImportOperation>, ImportError> {
        sqlx::query("SELECT * FROM import_operations WHERE id = ?")
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?
            .map(decode)
            .transpose()
    }

    pub(crate) async fn root_path(&self, id: &str) -> Result<PathBuf, ImportError> {
        let path: String = sqlx::query_scalar("SELECT path FROM library_roots WHERE id = ?")
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(ImportError::NotFound)?;
        Ok(path.into())
    }

    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T, ImportError> + Send + 'static,
    ) -> Result<T, ImportError> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ImportError::Worker)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f()
        })
        .await
        .map_err(|_| ImportError::Worker)?
    }

    pub(crate) async fn advance(
        &self,
        operation: &ImportOperation,
        phase: ImportPhase,
        staged: Option<&Identity>,
        policy: Option<&str>,
        validation: Option<&str>,
    ) -> Result<(), ImportError> {
        let staged = staged
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| ImportError::InvalidJournal)?;
        let mut tx = self.store.begin_write().await?;
        let changed = sqlx::query("UPDATE import_operations SET phase = ?, staged_identity_json = COALESCE(?, staged_identity_json), effective_policy = COALESCE(?, effective_policy), validation = COALESCE(?, validation), last_error = NULL, updated_at = unixepoch() WHERE id = ? AND phase = ?")
            .bind(phase.as_str()).bind(staged).bind(policy).bind(validation).bind(&operation.id).bind(operation.phase.as_str()).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(ImportError::Busy);
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn record_error(
        &self,
        id: &str,
        error: &ImportError,
    ) -> Result<(), ImportError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query(
            "UPDATE import_operations SET last_error = ?, updated_at = unixepoch() WHERE id = ?",
        )
        .bind(error.to_string())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

fn decode(row: sqlx::sqlite::SqliteRow) -> Result<ImportOperation, ImportError> {
    let phase = match row.get::<&str, _>("phase") {
        "planned" => ImportPhase::Planned,
        "staged" => ImportPhase::Staged,
        "verified" => ImportPhase::Verified,
        "finalized" => ImportPhase::Finalized,
        "cataloged" => ImportPhase::Cataloged,
        "cleanup_pending" => ImportPhase::CleanupPending,
        "done" => ImportPhase::Done,
        _ => return Err(ImportError::InvalidJournal),
    };
    Ok(ImportOperation {
        id: row.get("id"),
        phase,
        effective_policy: row.get("effective_policy"),
        validation: row.get("validation"),
        library_file_id: row.get("library_file_id"),
        last_error: row.get("last_error"),
        request: serde_json::from_str(row.get("request_json"))
            .map_err(|_| ImportError::InvalidJournal)?,
        identity: serde_json::from_str(row.get("identity_json"))
            .map_err(|_| ImportError::InvalidJournal)?,
        staged: row
            .get::<Option<String>, _>("staged_identity_json")
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|_| ImportError::InvalidJournal)?,
    })
}
