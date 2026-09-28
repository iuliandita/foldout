//! Durable selected-release orchestration. A committed submission fence is never reset.
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};
use uuid::Uuid;

use crate::{
    clients::{
        self, AuthorizedPayload, ClientError, ClientKind, DownloadState, OwnedJob,
        SubmissionAttempt,
    },
    importer::journal::{ImportPolicy, ImportService, InternalImportRequest},
    jobs::store::{canonical_json, enqueue_in_transaction, fingerprint, validate_label},
    search::selection::{SelectionError, SelectionRepository},
    search::{self, SearchError, digest, now, source_fingerprint},
    settings::{
        ClientOptions, Integration, IntegrationAdapter, IntegrationKind, IntegrationOptions,
        Settings, SettingsError,
    },
    store::sqlite::SqliteStore,
};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("invalid acquisition selection")]
    Invalid,
    #[error("acquisition was not found")]
    NotFound,
    #[error("selection, destination, or idempotency key conflicts with an existing intent")]
    Conflict,
    #[error(transparent)]
    Search(#[from] SearchError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error("acquisition database operation failed")]
    Database,
}
impl From<sqlx::Error> for PipelineError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation())
        {
            Self::Conflict
        } else {
            Self::Database
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DestinationSelection {
    pub root_id: String,
    pub relative_path: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionRequest {
    #[serde(alias = "releasehandle")]
    pub release_handle: String,
    pub client_id: String,
    pub unit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_decision_id: Option<String>,
    pub destination: Option<DestinationSelection>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAssociation {
    pub source_root_id: String,
    pub source_relative_path: String,
    pub destination: Option<DestinationSelection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionState {
    Queued,
    Downloading,
    Downloaded,
    Importing,
    Completed,
    NeedsReview,
    Canceled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionReason {
    UncertainSubmission,
    SourceUnavailable,
    ConfigurationChanged,
    UnsupportedTorrent,
    InvalidPayload,
    ClientUnavailable,
    ClientJobMissing,
    DownloadFailed,
    UnknownClientState,
    FileAssociationRequired,
    ImportReviewRequired,
    CancelRequiresReview,
    SelectionReviewRequired,
    DuplicateTorrent,
}
#[derive(Debug, Serialize)]
pub struct Acquisition {
    pub id: String,
    pub job_id: String,
    pub unit_id: String,
    pub state: AcquisitionState,
    pub reason: Option<AcquisitionReason>,
    pub submitted: bool,
    pub receipt_count: i64,
    pub import_id: Option<String>,
    pub destination_reserved: bool,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct Pipeline {
    store: SqliteStore,
}

#[derive(Serialize)]
pub struct RootChoice {
    pub id: String,
    pub label: String,
}

struct WorkClaim {
    id: String,
    token: String,
}

impl Pipeline {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn roots(&self) -> Result<Vec<RootChoice>, PipelineError> {
        sqlx::query("SELECT id, label FROM library_roots ORDER BY label, id")
            .fetch_all(self.store.reader())
            .await?
            .into_iter()
            .map(|row| {
                Ok(RootChoice {
                    id: row.try_get("id")?,
                    label: row.try_get("label")?,
                })
            })
            .collect()
    }

    /// No network writes. The request is the explicit authorization for one durable enqueue.
    pub async fn create(
        &self,
        settings: &Settings,
        owner: &str,
        key: &str,
        request: AcquisitionRequest,
    ) -> Result<Acquisition, PipelineError> {
        validate_label(owner).map_err(|_| PipelineError::Invalid)?;
        validate_label(key).map_err(|_| PipelineError::Invalid)?;
        for id in [
            &request.release_handle,
            &request.client_id,
            &request.unit_id,
        ] {
            valid_id(id)?;
        }
        if let Some(id) = &request.selection_decision_id {
            valid_id(id)?;
        }
        if let Some(destination) = &request.destination {
            validate_destination(destination)?;
        }
        let request_value = serde_json::to_value(&request).map_err(|_| PipelineError::Invalid)?;
        let request_text = canonical_json(&request_value).map_err(|_| PipelineError::Invalid)?;
        let request_fingerprint = fingerprint(&request_text);
        // An identical replay works after settings changes, handle expiry, or restart.
        if let Some(row) = sqlx::query("SELECT id, request_fingerprint FROM acquisition_intents WHERE caller = ? AND idempotency_key = ?").bind(owner).bind(key).fetch_optional(self.store.reader()).await? {
            if row.try_get::<String, _>("request_fingerprint")? != request_fingerprint { return Err(PipelineError::Conflict); }
            return self.get(owner, &row.try_get::<String, _>("id")?).await;
        }
        let selection_decision_id = request
            .selection_decision_id
            .as_deref()
            .ok_or(PipelineError::Invalid)?;
        let config = settings.load_private(&request.client_id).await?;
        if !config.integration.enabled || !config.integration.credentials_configured {
            return Err(SettingsError::NotConfigured.into());
        }
        let kind = client_kind(config.integration.kind)?;
        let IntegrationOptions::Client(options) = &config.integration.options else {
            return Err(PipelineError::Invalid);
        };
        let client_fingerprint = source_fingerprint(&config.integration)?;
        let mut tx = self.store.begin_write().await?;
        if let Some(row) = sqlx::query("SELECT id, request_fingerprint FROM acquisition_intents WHERE caller = ? AND idempotency_key = ?").bind(owner).bind(key).fetch_optional(&mut *tx).await? {
            if row.try_get::<String, _>("request_fingerprint")? != request_fingerprint { return Err(PipelineError::Conflict); }
            let id: String = row.try_get("id")?;
            tx.commit().await?;
            return self.get(owner, &id).await;
        }
        let release = sqlx::query("SELECT protocol, content_type FROM search_releases WHERE handle = ? AND owner = ? AND expires_at > unixepoch()")
            .bind(&request.release_handle).bind(owner).fetch_optional(&mut *tx).await?.ok_or(PipelineError::NotFound)?;
        let content: Option<String> = sqlx::query_scalar("SELECT p.content_type FROM units u JOIN editions e ON e.id = u.edition_id JOIN publications p ON p.id = e.publication_id WHERE u.id = ?")
            .bind(&request.unit_id).fetch_optional(&mut *tx).await?;
        if content.as_deref() != Some(release.try_get::<&str, _>("content_type")?) {
            return Err(PipelineError::Invalid);
        }
        if release.try_get::<&str, _>("protocol")?
            != match kind {
                ClientKind::Sabnzbd => "usenet",
                ClientKind::QBittorrent => "torrent",
            }
        {
            return Err(PipelineError::Invalid);
        }
        SelectionRepository::validate_selected_decision_in(
            &mut tx,
            owner,
            selection_decision_id,
            &request.unit_id,
            &request.release_handle,
        )
        .await?;
        let destination_path = match &request.destination {
            Some(destination) => Some(destination_path(&mut tx, destination).await?),
            None => None,
        };
        let id = Uuid::new_v4().to_string();
        // Job payloads contain only safe identifiers. Private paths remain in acquisition_runs.
        let job = enqueue_in_transaction(
            &mut tx,
            "acquisition.pipeline",
            &format!("acquisition:{owner}"),
            serde_json::json!({"acquisition_id": id}),
        )
        .await
        .map_err(|_| PipelineError::Database)?;
        sqlx::query("INSERT INTO acquisition_intents (id, caller, idempotency_key, request, request_fingerprint, job_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id).bind(owner).bind(key).bind(serde_json::json!({"release_handle":request.release_handle,"client_id":request.client_id,"unit_id":request.unit_id,"selection_decision_id":selection_decision_id}).to_string())
            .bind(request_fingerprint).bind(&job.id).bind(now()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO acquisition_runs (id, release_handle, client_id, client_fingerprint, client_kind, category, unit_id, state, destination_root, destination_relative, destination_path, selection_decision_id) VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?)")
            .bind(&id).bind(&request.release_handle).bind(&request.client_id).bind(client_fingerprint).bind(kind_name(kind)).bind(&options.category).bind(&request.unit_id)
            .bind(request.destination.as_ref().map(|d| &d.root_id)).bind(request.destination.as_ref().map(|d| &d.relative_path)).bind(destination_path).bind(selection_decision_id).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(owner, &id).await
    }

    pub async fn get(&self, owner: &str, id: &str) -> Result<Acquisition, PipelineError> {
        let row = sqlx::query("SELECT a.*, i.job_id, (SELECT COUNT(*) FROM acquisition_receipts r WHERE r.acquisition_id = a.id) AS receipt_count FROM acquisition_runs a JOIN acquisition_intents i ON i.id = a.id WHERE a.id = ? AND i.caller = ?")
            .bind(id).bind(owner).fetch_optional(self.store.reader()).await?.ok_or(PipelineError::NotFound)?;
        view(row)
    }
    pub async fn list(
        &self,
        owner: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Acquisition>, PipelineError> {
        if limit == 0 || limit > 100 {
            return Err(PipelineError::Invalid);
        }
        let rows = sqlx::query("SELECT a.*, i.job_id, (SELECT COUNT(*) FROM acquisition_receipts r WHERE r.acquisition_id = a.id) AS receipt_count FROM acquisition_runs a JOIN acquisition_intents i ON i.id = a.id WHERE i.caller = ? ORDER BY a.created_at DESC, a.id DESC LIMIT ? OFFSET ?")
            .bind(owner).bind(limit).bind(offset).fetch_all(self.store.reader()).await?;
        rows.into_iter().map(view).collect()
    }

    /// The caller explicitly associates a local file; remote paths and fuzzy names are never adopted.
    pub async fn associate_file(
        &self,
        owner: &str,
        id: &str,
        association: FileAssociation,
    ) -> Result<Acquisition, PipelineError> {
        valid_id(&association.source_root_id)?;
        relative(&association.source_relative_path)?;
        if let Some(destination) = &association.destination {
            validate_destination(destination)?;
        }
        let mut tx = self.store.begin_write().await?;
        let row = sqlx::query("SELECT a.* FROM acquisition_runs a JOIN acquisition_intents i ON i.id = a.id WHERE a.id = ? AND i.caller = ?")
            .bind(id).bind(owner).fetch_optional(&mut *tx).await?.ok_or(PipelineError::NotFound)?;
        let destination = match association.destination {
            Some(destination) => destination,
            None => DestinationSelection {
                root_id: row
                    .try_get::<Option<String>, _>("destination_root")?
                    .ok_or(PipelineError::Invalid)?,
                relative_path: row
                    .try_get::<Option<String>, _>("destination_relative")?
                    .ok_or(PipelineError::Invalid)?,
            },
        };
        if let Some(import_id) = row.try_get::<Option<String>, _>("import_id")? {
            let same = row.try_get::<Option<String>, _>("source_root")?.as_deref()
                == Some(&association.source_root_id)
                && row
                    .try_get::<Option<String>, _>("source_relative")?
                    .as_deref()
                    == Some(&association.source_relative_path)
                && row
                    .try_get::<Option<String>, _>("destination_root")?
                    .as_deref()
                    == Some(&destination.root_id)
                && row
                    .try_get::<Option<String>, _>("destination_relative")?
                    .as_deref()
                    == Some(&destination.relative_path);
            if !same || import_id.is_empty() {
                return Err(PipelineError::Conflict);
            }
            tx.commit().await?;
            return self.get(owner, id).await;
        }
        if row.try_get::<&str, _>("state")? != "downloaded" {
            return Err(PipelineError::Conflict);
        }
        let source_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_roots WHERE id = ?)")
                .bind(&association.source_root_id)
                .fetch_one(&mut *tx)
                .await?;
        if !source_exists {
            return Err(PipelineError::NotFound);
        }
        if let Some(root) = row.try_get::<Option<String>, _>("destination_root")?
            && (root != destination.root_id
                || row
                    .try_get::<Option<String>, _>("destination_relative")?
                    .as_deref()
                    != Some(&destination.relative_path))
        {
            return Err(PipelineError::Conflict);
        }
        let path = destination_path(&mut tx, &destination).await?;
        sqlx::query("UPDATE acquisition_runs SET source_root = ?, source_relative = ?, destination_root = ?, destination_relative = ?, destination_path = ?, import_id = ?, state = 'importing', reason = NULL, next_poll_at = 0, updated_at = unixepoch() WHERE id = ?")
            .bind(association.source_root_id).bind(association.source_relative_path).bind(destination.root_id).bind(destination.relative_path).bind(path)
            .bind(Uuid::new_v4().to_string()).bind(id).execute(&mut *tx).await?;
        job_state(&mut tx, id, "running", "file_associated", None).await?;
        tx.commit().await?;
        self.get(owner, id).await
    }

    /// At most one submission, one receipt status, or one import journal phase per call.
    /// Run in its own worker task. No DB transaction spans a provider/client/filesystem await.
    pub async fn tick(&self, settings: Settings) -> Result<bool, PipelineError> {
        let mut tx = self.store.begin_write().await?;
        let row = sqlx::query("SELECT id FROM acquisition_runs WHERE state IN ('queued', 'downloading', 'importing') AND next_poll_at <= unixepoch() ORDER BY next_poll_at, created_at, id LIMIT 1")
            .fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let id: String = row.try_get("id")?;
        sqlx::query("UPDATE acquisition_runs SET next_poll_at = unixepoch() + 180 WHERE id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.tick_job(settings, &id).await?;
        Ok(true)
    }

    pub async fn tick_job(&self, settings: Settings, id: &str) -> Result<(), PipelineError> {
        let claim = WorkClaim {
            id: id.to_string(),
            token: Uuid::new_v4().to_string(),
        };
        let mut tx = self.store.begin_write().await?;
        let acquired = sqlx::query("UPDATE acquisition_runs SET work_token = ?, work_until = unixepoch() + 300 WHERE id = ? AND state IN ('queued', 'downloading', 'importing') AND (work_until IS NULL OR work_until <= unixepoch())")
            .bind(&claim.token).bind(id).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        if !acquired {
            return Ok(());
        }
        let result = self.process_job(settings, &claim).await;
        let mut tx = self.store.begin_write().await?;
        sqlx::query("UPDATE acquisition_runs SET work_token = NULL, work_until = NULL WHERE id = ? AND work_token = ?")
            .bind(id).bind(&claim.token).execute(&mut *tx).await?;
        tx.commit().await?;
        result
    }

    async fn process_job(
        &self,
        settings: Settings,
        claim: &WorkClaim,
    ) -> Result<(), PipelineError> {
        let id = claim.id.as_str();
        let row = sqlx::query("SELECT a.*, i.caller, j.state AS job_state FROM acquisition_runs a JOIN acquisition_intents i ON i.id = a.id JOIN jobs j ON j.id = i.job_id WHERE a.id = ?")
            .bind(id).fetch_optional(self.store.reader()).await?.ok_or(PipelineError::NotFound)?;
        let state: &str = row.try_get("state")?;
        if !matches!(state, "queued" | "downloading" | "importing") {
            return Ok(());
        }
        if matches!(
            row.try_get::<&str, _>("job_state")?,
            "cancel_requested" | "canceled"
        ) {
            if !row.try_get::<bool, _>("attempted")? {
                return self.set_state(claim, "canceled", None).await;
            }
            return self
                .set_state(claim, "needs_review", Some("cancel_requires_review"))
                .await;
        }
        if state == "importing" {
            return self.import_step(claim, &row).await;
        }
        let client_id: String = row.try_get("client_id")?;
        let (adapter, current_fingerprint) = match client_adapter(&settings, &client_id).await {
            Ok(value) => value,
            Err(_) => {
                return self
                    .set_state(claim, "needs_review", Some("configuration_changed"))
                    .await;
            }
        };
        if current_fingerprint != row.try_get::<String, _>("client_fingerprint")? {
            return self
                .set_state(claim, "needs_review", Some("configuration_changed"))
                .await;
        }
        if state == "downloading" {
            return self.poll_receipt(claim, &row, adapter).await;
        }
        if row.try_get::<bool, _>("attempted")? {
            return self
                .set_state(claim, "needs_review", Some("uncertain_submission"))
                .await;
        }
        let owner: String = row.try_get("caller")?;
        let decision_id: Option<String> = row.try_get("selection_decision_id")?;
        let unit_id: String = row.try_get("unit_id")?;
        let handle: String = row.try_get("release_handle")?;
        if !self
            .selection_authorized(
                claim,
                &owner,
                decision_id.as_deref(),
                &unit_id,
                &handle,
                None,
            )
            .await?
        {
            return Ok(());
        }
        let (bytes, protocol, fresh_evidence) =
            match search::selected_payload(&self.store, &settings, &handle).await {
                Ok(value) => value,
                Err(SearchError::Cooldown {
                    retry_after_seconds,
                }) => return self.defer(claim, retry_after_seconds).await,
                Err(SearchError::Provider(crate::providers::ProviderError::RateLimited {
                    retry_after_seconds,
                })) => return self.defer(claim, retry_after_seconds.unwrap_or(60)).await,
                Err(
                    SearchError::Changed
                    | SearchError::Settings(SettingsError::NotFound | SettingsError::NotConfigured),
                ) => {
                    return self
                        .set_state(claim, "needs_review", Some("selection_review_required"))
                        .await;
                }
                Err(_) => {
                    return self
                        .set_state(claim, "needs_review", Some("source_unavailable"))
                        .await;
                }
            };
        let payload_digest = digest(&bytes);
        let (payload, hash) = match protocol {
            crate::providers::ReleaseProtocol::Usenet => (AuthorizedPayload::nzb(bytes), None),
            crate::providers::ReleaseProtocol::Torrent => {
                let hash = match search::torrent::v1_infohash(&bytes) {
                    Ok(hash) => hash,
                    Err(search::torrent::TorrentError::UnsupportedV2) => {
                        return self
                            .set_state(claim, "needs_review", Some("unsupported_torrent"))
                            .await;
                    }
                    Err(_) => {
                        return self
                            .set_state(claim, "needs_review", Some("invalid_payload"))
                            .await;
                    }
                };
                (AuthorizedPayload::torrent(bytes, hash.clone()), Some(hash))
            }
        };
        let payload = match payload {
            Ok(payload) => payload,
            Err(_) => {
                return self
                    .set_state(claim, "needs_review", Some("invalid_payload"))
                    .await;
            }
        };
        // This transaction is the sole authority to mutate. It also serializes client/hash ownership.
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(());
        }
        if !selection_authorized_in(
            &mut tx,
            id,
            &owner,
            decision_id.as_deref(),
            &unit_id,
            &handle,
            Some(&fresh_evidence),
        )
        .await?
        {
            tx.commit().await?;
            return Ok(());
        }
        if !client_configuration_current_in(
            &mut tx,
            &settings,
            &client_id,
            row.try_get("client_kind")?,
            row.try_get("client_fingerprint")?,
        )
        .await?
        {
            sqlx::query("UPDATE acquisition_runs SET state = 'needs_review', reason = 'configuration_changed', updated_at = unixepoch() WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            job_state(
                &mut tx,
                id,
                "needs_review",
                "client_configuration_changed",
                Some("configuration_changed"),
            )
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        let changed = sqlx::query("UPDATE acquisition_runs SET attempted = 1, payload_digest = ?, torrent_hash = ?, state = 'needs_review', reason = 'uncertain_submission', updated_at = unixepoch() WHERE id = ? AND state = 'queued' AND attempted = 0 AND EXISTS(SELECT 1 FROM acquisition_intents i JOIN jobs j ON j.id = i.job_id WHERE i.id = acquisition_runs.id AND j.state IN ('queued', 'retry_wait', 'running'))")
            .bind(payload_digest).bind(hash).bind(id).execute(&mut *tx).await;
        let changed = match changed {
            Ok(result) => result.rows_affected(),
            Err(error)
                if error
                    .as_database_error()
                    .is_some_and(|e| e.is_unique_violation()) =>
            {
                tx.rollback().await?;
                return self
                    .set_state(claim, "needs_review", Some("duplicate_torrent"))
                    .await;
            }
            Err(error) => return Err(error.into()),
        };
        if changed != 1 {
            tx.commit().await?;
            return Ok(());
        }
        job_state(
            &mut tx,
            id,
            "needs_review",
            "submission_fenced",
            Some("uncertain_submission"),
        )
        .await?;
        tx.commit().await?;
        let own_id = Uuid::parse_str(id).map_err(|_| PipelineError::Database)?;
        let mut attempt = SubmissionAttempt::from_persisted(own_id, false)
            .map_err(|_| PipelineError::Database)?;
        let result = match adapter {
            IntegrationAdapter::Sabnzbd(client) => client.enqueue(&mut attempt, payload).await,
            IntegrationAdapter::QBittorrent(client) => client.enqueue(&mut attempt, payload).await,
            _ => return Err(PipelineError::Invalid),
        };
        // Any error leaves the durable fence untouched, even if the adapter failed preflight.
        let Ok(receipts) = result else {
            return Ok(());
        };
        self.record_receipts(claim, receipts).await
    }

    async fn selection_authorized(
        &self,
        claim: &WorkClaim,
        owner: &str,
        decision_id: Option<&str>,
        unit_id: &str,
        release_handle: &str,
        fresh_evidence: Option<&crate::providers::release_evidence::ReleaseEvidence>,
    ) -> Result<bool, PipelineError> {
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(false);
        }
        let authorized = selection_authorized_in(
            &mut tx,
            claim.id.as_str(),
            owner,
            decision_id,
            unit_id,
            release_handle,
            fresh_evidence,
        )
        .await?;
        tx.commit().await?;
        Ok(authorized)
    }

    async fn record_receipts(
        &self,
        claim: &WorkClaim,
        receipts: Vec<OwnedJob>,
    ) -> Result<(), PipelineError> {
        let id = claim.id.as_str();
        if receipts.is_empty() || receipts.iter().any(|r| r.own_id().to_string() != id) {
            return Err(PipelineError::Database);
        }
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(());
        }
        for (ordinal, receipt) in receipts.iter().enumerate() {
            sqlx::query("INSERT INTO acquisition_receipts (acquisition_id, ordinal, external_id) VALUES (?, ?, ?)").bind(id).bind(ordinal as i64).bind(receipt.external_id()).execute(&mut *tx).await?;
        }
        let canceled = cancellation_requested(&mut tx, id).await?;
        let reason = canceled.then_some("cancel_requires_review");
        sqlx::query("UPDATE acquisition_runs SET state = ?, reason = ?, next_poll_at = 0, updated_at = unixepoch() WHERE id = ? AND attempted = 1")
            .bind(if canceled { "needs_review" } else { "downloading" }).bind(reason).bind(id).execute(&mut *tx).await?;
        job_state(
            &mut tx,
            id,
            if canceled { "needs_review" } else { "running" },
            "client_accepted",
            reason,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn poll_receipt(
        &self,
        claim: &WorkClaim,
        row: &sqlx::sqlite::SqliteRow,
        adapter: IntegrationAdapter,
    ) -> Result<(), PipelineError> {
        let id = claim.id.as_str();
        let cursor: i64 = row.try_get("poll_cursor")?;
        let receipt = sqlx::query("SELECT ordinal, external_id FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY (ordinal < ?), ordinal LIMIT 1")
            .bind(id).bind(cursor).fetch_optional(self.store.reader()).await?.ok_or(PipelineError::Database)?;
        let kind = match row.try_get::<&str, _>("client_kind")? {
            "sabnzbd" => ClientKind::Sabnzbd,
            "qbittorrent" => ClientKind::QBittorrent,
            _ => return Err(PipelineError::Database),
        };
        let job = OwnedJob::from_persisted_receipt(
            Uuid::parse_str(id).map_err(|_| PipelineError::Database)?,
            Uuid::parse_str(row.try_get("client_id")?).map_err(|_| PipelineError::Database)?,
            kind,
            row.try_get("category")?,
            receipt.try_get("external_id")?,
        )
        .map_err(|_| PipelineError::Database)?;
        let result = match adapter {
            IntegrationAdapter::Sabnzbd(client) => client.status(&job).await,
            IntegrationAdapter::QBittorrent(client) => client.status(&job).await,
            _ => return Err(PipelineError::Invalid),
        };
        let completed = match result {
            Ok(status) => match status.state {
                DownloadState::Completed | DownloadState::Seeding => true,
                DownloadState::Queued
                | DownloadState::Downloading
                | DownloadState::Paused
                | DownloadState::Processing => false,
                DownloadState::Failed => {
                    return self
                        .set_state(claim, "needs_review", Some("download_failed"))
                        .await;
                }
                DownloadState::Unknown => {
                    return self
                        .set_state(claim, "needs_review", Some("unknown_client_state"))
                        .await;
                }
            },
            Err(ClientError::NotFound | ClientError::NotOwned) => {
                return self
                    .set_state(claim, "needs_review", Some("client_job_missing"))
                    .await;
            }
            Err(_) => {
                return self
                    .set_state(claim, "needs_review", Some("client_unavailable"))
                    .await;
            }
        };
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(());
        }
        let ordinal: i64 = receipt.try_get("ordinal")?;
        if cancellation_requested(&mut tx, id).await? {
            sqlx::query("UPDATE acquisition_runs SET state = 'needs_review', reason = 'cancel_requires_review', updated_at = unixepoch() WHERE id = ?")
                .bind(id).execute(&mut *tx).await?;
            job_state(
                &mut tx,
                id,
                "needs_review",
                "cancel_after_submission",
                Some("cancel_requires_review"),
            )
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query("UPDATE acquisition_receipts SET completed = ? WHERE acquisition_id = ? AND ordinal = ?").bind(completed).bind(id).bind(ordinal).execute(&mut *tx).await?;
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM acquisition_receipts WHERE acquisition_id = ? AND completed = 0",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE acquisition_runs SET state = ?, reason = ?, poll_cursor = ?, next_poll_at = unixepoch() + 5, updated_at = unixepoch() WHERE id = ? AND state = 'downloading'")
            .bind(if pending == 0 { "downloaded" } else { "downloading" }).bind(if pending == 0 { Some("file_association_required") } else { None }).bind(ordinal + 1).bind(id).execute(&mut *tx).await?;
        if pending == 0 {
            job_state(
                &mut tx,
                id,
                "needs_review",
                "download_completed",
                Some("file_association_required"),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn import_step(
        &self,
        claim: &WorkClaim,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<(), PipelineError> {
        let import_id: String = row.try_get("import_id")?;
        let request = InternalImportRequest {
            source_root: row.try_get("source_root")?,
            source_relative: row.try_get("source_relative")?,
            destination_root: row.try_get("destination_root")?,
            destination_relative: row.try_get("destination_relative")?,
            unit_id: row.try_get("unit_id")?,
            policy: ImportPolicy::Copy,
        };
        let service = ImportService::new(self.store.clone());
        match service.plan(&import_id, request).await {
            Ok(_) => {}
            Err(crate::importer::journal::ImportError::Busy) => return self.defer(claim, 5).await,
            Err(_) => {
                return self
                    .set_state(claim, "needs_review", Some("import_review_required"))
                    .await;
            }
        }
        match service.step(&import_id).await {
            Ok(operation)
                if operation.phase == crate::importer::journal::ImportPhase::Done
                    && operation.library_file_id.is_some() =>
            {
                self.set_state(claim, "completed", None).await
            }
            Ok(_) | Err(crate::importer::journal::ImportError::Busy) => self.defer(claim, 1).await,
            Err(_) => {
                self.set_state(claim, "needs_review", Some("import_review_required"))
                    .await
            }
        }
    }

    async fn defer(&self, claim: &WorkClaim, seconds: u64) -> Result<(), PipelineError> {
        let id = claim.id.as_str();
        let seconds = seconds.max(1).min(i64::MAX as u64 - now() as u64) as i64;
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query("UPDATE acquisition_runs SET next_poll_at = ? WHERE id = ?")
            .bind(now() + seconds)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn set_state(
        &self,
        claim: &WorkClaim,
        state: &str,
        reason: Option<&str>,
    ) -> Result<(), PipelineError> {
        let id = claim.id.as_str();
        let mut tx = self.store.begin_write().await?;
        if !owns_claim(&mut tx, claim).await? {
            tx.commit().await?;
            return Ok(());
        }
        if state == "needs_review" && cancel_before_submission(&mut tx, id).await? {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query("UPDATE acquisition_runs SET state = ?, reason = ?, updated_at = unixepoch() WHERE id = ?").bind(state).bind(reason).bind(id).execute(&mut *tx).await?;
        job_state(
            &mut tx,
            id,
            match state {
                "completed" => "completed",
                "canceled" => "canceled",
                _ => "needs_review",
            },
            "acquisition_state",
            reason,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn selection_authorized_in(
    tx: &mut Transaction<'_, Sqlite>,
    acquisition_id: &str,
    owner: &str,
    decision_id: Option<&str>,
    unit_id: &str,
    release_handle: &str,
    fresh_evidence: Option<&crate::providers::release_evidence::ReleaseEvidence>,
) -> Result<bool, PipelineError> {
    if cancel_before_submission(tx, acquisition_id).await? {
        return Ok(false);
    }
    let assessment = match decision_id {
        Some(decision_id) => {
            SelectionRepository::validate_selected_decision_in(
                tx,
                owner,
                decision_id,
                unit_id,
                release_handle,
            )
            .await
        }
        None => Err(SelectionError::NotFound),
    };
    let authorized = match assessment {
        Ok(assessment) => fresh_evidence.is_none_or(|fresh| *fresh == assessment.evidence),
        Err(SelectionError::Database) => return Err(PipelineError::Database),
        Err(_) => false,
    };
    if authorized {
        return Ok(true);
    }
    sqlx::query("UPDATE acquisition_runs SET state = 'needs_review', reason = 'selection_review_required', updated_at = unixepoch() WHERE id = ?")
        .bind(acquisition_id)
        .execute(&mut **tx)
        .await?;
    job_state(
        tx,
        acquisition_id,
        "needs_review",
        "selection_invalidated",
        Some("selection_review_required"),
    )
    .await?;
    Ok(false)
}

async fn client_configuration_current_in(
    tx: &mut Transaction<'_, Sqlite>,
    settings: &Settings,
    client_id: &str,
    saved_kind: &str,
    saved_fingerprint: &str,
) -> Result<bool, PipelineError> {
    let Some(row) = sqlx::query(
        "SELECT kind, label, base_url, enabled, options FROM integrations WHERE id = ?",
    )
    .bind(client_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(false);
    };
    let kind_name: String = row.try_get("kind")?;
    if !row.try_get::<bool, _>("enabled")? || kind_name != saved_kind {
        return Ok(false);
    }
    let kind = match kind_name.as_str() {
        "sabnzbd" => IntegrationKind::Sabnzbd,
        "qbittorrent" => IntegrationKind::QBittorrent,
        _ => return Ok(false),
    };
    let options: ClientOptions =
        serde_json::from_str(row.try_get("options")?).map_err(|_| PipelineError::Database)?;
    let current = Integration {
        id: client_id.to_string(),
        kind,
        label: row.try_get("label")?,
        base_url: row.try_get("base_url")?,
        enabled: true,
        options: IntegrationOptions::Client(options),
        api_key_configured: false,
        username_configured: false,
        password_configured: false,
        credentials_configured: false,
    };
    if source_fingerprint(&current)? != saved_fingerprint {
        return Ok(false);
    }
    // The held write lock keeps this committed reader snapshot current.
    match settings.load_private(client_id).await {
        Ok(private) => Ok(private.integration.credentials_configured),
        Err(SettingsError::Database) => Err(PipelineError::Database),
        Err(_) => Ok(false),
    }
}

async fn cancellation_requested(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> Result<bool, PipelineError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM acquisition_intents i JOIN jobs j ON j.id = i.job_id WHERE i.id = ? AND j.state IN ('cancel_requested', 'canceled'))")
        .bind(id).fetch_one(&mut **tx).await?)
}

/// A pending cancel wins over any review outcome until the submission fence is committed.
async fn cancel_before_submission(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> Result<bool, PipelineError> {
    if !cancellation_requested(tx, id).await? {
        return Ok(false);
    }
    let canceled = sqlx::query("UPDATE acquisition_runs SET state = 'canceled', reason = NULL, updated_at = unixepoch() WHERE id = ? AND attempted = 0")
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        == 1;
    if canceled {
        job_state(tx, id, "canceled", "cancel_before_submission", None).await?;
    }
    Ok(canceled)
}

async fn owns_claim(
    tx: &mut Transaction<'_, Sqlite>,
    claim: &WorkClaim,
) -> Result<bool, PipelineError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM acquisition_runs WHERE id = ? AND work_token = ?)",
    )
    .bind(&claim.id)
    .bind(&claim.token)
    .fetch_one(&mut **tx)
    .await?)
}

async fn client_adapter(
    settings: &Settings,
    id: &str,
) -> Result<(IntegrationAdapter, String), PipelineError> {
    let private = settings.load_private(id).await?;
    let integration = private.integration;
    if !integration.enabled || !integration.credentials_configured {
        return Err(SettingsError::NotConfigured.into());
    }
    let fingerprint = source_fingerprint(&integration)?;
    let IntegrationOptions::Client(options) = integration.options else {
        return Err(PipelineError::Invalid);
    };
    let config = clients::ClientConfig::new(
        Uuid::parse_str(id).map_err(|_| PipelineError::Invalid)?,
        &integration.base_url,
        options.category,
        clients::HttpLimits::default(),
    )
    .map_err(|_| PipelineError::Invalid)?;
    let adapter = match integration.kind {
        IntegrationKind::Sabnzbd => IntegrationAdapter::Sabnzbd(
            clients::Sabnzbd::new(config, private.api_key.ok_or(PipelineError::Invalid)?)
                .map_err(|_| PipelineError::Invalid)?,
        ),
        IntegrationKind::QBittorrent => IntegrationAdapter::QBittorrent(
            clients::QBittorrent::new(
                config,
                private.username.ok_or(PipelineError::Invalid)?,
                private.password.ok_or(PipelineError::Invalid)?,
            )
            .map_err(|_| PipelineError::Invalid)?,
        ),
        _ => return Err(PipelineError::Invalid),
    };
    Ok((adapter, fingerprint))
}
async fn job_state(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    state: &str,
    event: &str,
    reason: Option<&str>,
) -> Result<(), PipelineError> {
    let row = sqlx::query("SELECT j.id, j.state FROM jobs j JOIN acquisition_intents i ON i.job_id = j.id WHERE i.id = ?").bind(id).fetch_one(&mut **tx).await?;
    let job_id: String = row.try_get("id")?;
    let before: String = row.try_get("state")?;
    sqlx::query("UPDATE jobs SET state = ?, reason = ?, worker = NULL, lease_until = NULL, retry_at = NULL, updated_at = unixepoch() WHERE id = ?")
        .bind(state).bind(reason).bind(&job_id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO job_events (job_id, event, from_state, to_state, reason, created_at) VALUES (?, ?, ?, ?, ?, unixepoch())")
        .bind(job_id).bind(event).bind(before).bind(state).bind(reason).execute(&mut **tx).await?;
    Ok(())
}
fn view(row: sqlx::sqlite::SqliteRow) -> Result<Acquisition, PipelineError> {
    let state = serde_json::from_value(serde_json::Value::String(row.try_get("state")?))
        .map_err(|_| PipelineError::Database)?;
    let reason = row
        .try_get::<Option<String>, _>("reason")?
        .map(|s| serde_json::from_value(serde_json::Value::String(s)))
        .transpose()
        .map_err(|_| PipelineError::Database)?;
    Ok(Acquisition {
        id: row.try_get("id")?,
        job_id: row.try_get("job_id")?,
        unit_id: row.try_get("unit_id")?,
        state,
        reason,
        submitted: row.try_get("attempted")?,
        receipt_count: row.try_get("receipt_count")?,
        import_id: row.try_get("import_id")?,
        destination_reserved: row
            .try_get::<Option<String>, _>("destination_root")?
            .is_some(),
        updated_at: row.try_get("updated_at")?,
    })
}
fn kind_name(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::Sabnzbd => "sabnzbd",
        ClientKind::QBittorrent => "qbittorrent",
    }
}
fn client_kind(kind: IntegrationKind) -> Result<ClientKind, PipelineError> {
    match kind {
        IntegrationKind::Sabnzbd => Ok(ClientKind::Sabnzbd),
        IntegrationKind::QBittorrent => Ok(ClientKind::QBittorrent),
        _ => Err(PipelineError::Invalid),
    }
}
fn valid_id(id: &str) -> Result<(), PipelineError> {
    if Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id) {
        Ok(())
    } else {
        Err(PipelineError::Invalid)
    }
}
fn relative(path: &str) -> Result<(), PipelineError> {
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || path.contains('\\')
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        Err(PipelineError::Invalid)
    } else {
        Ok(())
    }
}
fn validate_destination(destination: &DestinationSelection) -> Result<(), PipelineError> {
    valid_id(&destination.root_id)?;
    relative(&destination.relative_path)
}
async fn destination_path(
    tx: &mut Transaction<'_, Sqlite>,
    destination: &DestinationSelection,
) -> Result<String, PipelineError> {
    let root: String = sqlx::query_scalar("SELECT path FROM library_roots WHERE id = ?")
        .bind(&destination.root_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(PipelineError::NotFound)?;
    Ok(Path::new(&root)
        .join(&destination.relative_path)
        .to_str()
        .ok_or(PipelineError::Invalid)?
        .to_string())
}

#[cfg(test)]
#[path = "pipeline_test.rs"]
mod tests;
