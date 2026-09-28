use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    catalog::{
        ContentType,
        wanted::{UnitContext, WantedRepository},
    },
    providers::release_evidence::{
        Evidence, ReleaseEvidence, parse_release_evidence, safe_release_title,
    },
    providers::{MetadataPage, ProviderError, ReleaseProtocol, SearchPage},
    search::selection::{
        AssessmentInput, Decision, ReleaseIdentity, SelectionError, SelectionRepository,
    },
    settings::{
        Integration, IntegrationAdapter, IntegrationKind, IntegrationOptions, PrivateIntegration,
        Protocol, Settings, SettingsError,
    },
    store::sqlite::SqliteStore,
};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid search request")]
    Invalid,
    #[error("selected release is unavailable; search again")]
    NotFound,
    #[error("catalog unit was not found")]
    UnitNotFound,
    #[error("catalog target or selection policy changed; search again")]
    TargetChanged,
    #[error("selected integration does not support this operation")]
    Unsupported,
    #[error("integration configuration changed; search again")]
    Changed,
    #[error("release has an active rejection for this unit and policy")]
    ReleaseRejected,
    #[error("search is cooling down")]
    Cooldown { retry_after_seconds: u64 },
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("search database operation failed")]
    Database,
}
impl From<sqlx::Error> for SearchError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSearch {
    pub integration_id: String,
    pub content_type: ContentType,
    pub query: String,
    #[serde(default)]
    pub unit_id: Option<String>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAssessmentRequest {
    pub release_handle: String,
    pub unit_id: String,
}

#[derive(Serialize)]
pub struct ReleaseAssessmentResponse {
    pub assessment_id: String,
    pub assessment_expires_at: i64,
    pub evaluation: crate::search::matching::Evaluation,
    pub active_rejection: Option<Decision>,
    pub target: UnitContext,
}
fn default_limit() -> u32 {
    20
}

#[derive(Serialize)]
pub struct Release {
    pub release_handle: String,
    pub integration_id: String,
    pub indexer_id: u32,
    pub title: String,
    pub content_type: ContentType,
    pub categories: Vec<u32>,
    pub size_bytes: u64,
    pub protocol: ReleaseProtocol,
    pub evidence: ReleaseEvidence,
    pub evaluation: Option<crate::search::matching::Evaluation>,
    pub assessment_id: Option<String>,
    pub assessment_expires_at: Option<i64>,
    pub active_rejection: Option<Decision>,
}
#[derive(Serialize)]
pub struct Releases {
    pub releases: Vec<Release>,
    pub offset: u32,
    pub total: Option<u64>,
    pub next_offset: Option<u32>,
    pub target: Option<UnitContext>,
}

#[derive(Serialize)]
pub struct IntegrationChoice {
    pub id: String,
    pub kind: IntegrationKind,
    pub label: String,
    pub metadata_lookup: bool,
    pub release_search: bool,
    pub download_client: bool,
    pub content_types: Vec<ContentType>,
    pub protocol: Option<Protocol>,
}

type MetadataCache = Arc<Mutex<HashMap<String, (i64, MetadataPage)>>>;
#[derive(Clone)]
pub struct Search {
    store: SqliteStore,
    settings: Settings,
    metadata: MetadataCache,
}

impl Search {
    pub fn new(store: SqliteStore, settings: Settings) -> Self {
        Self {
            store,
            settings,
            metadata: Arc::default(),
        }
    }

    pub async fn integrations(&self) -> Result<Vec<IntegrationChoice>, SearchError> {
        let mut items = Vec::new();
        for integration in self.settings.list().await? {
            if !integration.enabled || !integration.credentials_configured {
                continue;
            }
            let mut content_types = Vec::new();
            let (metadata_lookup, release_search, download_client, protocol) =
                match (&integration.kind, &integration.options) {
                    (IntegrationKind::ComicVine, _) => {
                        content_types.push(ContentType::Comic);
                        (true, false, false, None)
                    }
                    (IntegrationKind::MangaUpdates | IntegrationKind::MangaDex, _) => {
                        content_types.push(ContentType::Manga);
                        (true, false, false, None)
                    }
                    (IntegrationKind::GetComics, _) => {
                        content_types.push(ContentType::Comic);
                        (false, false, false, None)
                    }
                    (IntegrationKind::InternetArchive, _) => {
                        content_types.push(ContentType::Magazine);
                        (false, false, false, None)
                    }
                    (IntegrationKind::Prowlarr, IntegrationOptions::Prowlarr(options)) => {
                        if !options.categories.comics.is_empty() {
                            content_types.push(ContentType::Comic);
                        }
                        if !options.categories.manga.is_empty() {
                            content_types.push(ContentType::Manga);
                        }
                        if !options.categories.magazines.is_empty() {
                            content_types.push(ContentType::Magazine);
                        }
                        (false, true, false, Some(options.protocol))
                    }
                    (IntegrationKind::Sabnzbd, _) => (false, false, true, Some(Protocol::Usenet)),
                    (IntegrationKind::QBittorrent, _) => {
                        (false, false, true, Some(Protocol::Torrent))
                    }
                    _ => continue,
                };
            items.push(IntegrationChoice {
                id: integration.id,
                kind: integration.kind,
                label: integration.label,
                metadata_lookup,
                release_search,
                download_client,
                content_types,
                protocol,
            });
        }
        Ok(items)
    }

    pub async fn metadata(
        &self,
        integration_id: &str,
        query: &str,
        page: u32,
        limit: u32,
    ) -> Result<MetadataPage, SearchError> {
        valid_query(query)?;
        if page == 0 || limit == 0 || limit > 100 {
            return Err(SearchError::Invalid);
        }
        let private = self.settings.load_private(integration_id).await?;
        let integration = &private.integration;
        if !integration.enabled || !integration.credentials_configured {
            return Err(SettingsError::NotConfigured.into());
        }
        let key = digest(
            format!(
                "{}:{query}:{page}:{limit}",
                source_fingerprint(integration)?
            )
            .as_bytes(),
        );
        if let Some((expires, result)) = self.metadata.lock().await.get(&key)
            && *expires > now()
        {
            return Ok(result.clone());
        }
        // A shared credential/base scope also coordinates duplicate integration records.
        let scope = credential_scope(integration, private.api_key.as_deref());
        reserve_cooldown(
            &self.store,
            &scope,
            if integration.kind == IntegrationKind::ComicVine {
                20
            } else {
                1
            },
        )
        .await?;
        let page = SearchPage {
            number: page,
            size: limit,
        };
        let result = match self.settings.adapter(integration_id).await? {
            IntegrationAdapter::ComicVine(adapter) => adapter.search(query, page).await,
            IntegrationAdapter::MangaUpdates(adapter) => adapter.search(query, page).await,
            IntegrationAdapter::MangaDex(adapter) => adapter.search(query, page).await,
            _ => return Err(SearchError::Unsupported),
        };
        let mut result = provider_result(&self.store, &scope, result).await?;
        for candidate in &mut result.candidates {
            candidate.title = display_title(&candidate.title);
        }
        let mut cache = self.metadata.lock().await;
        cache.retain(|_, (expires, _)| *expires > now());
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(key, (now() + 900, result.clone()));
        Ok(result)
    }

    pub async fn archive(
        &self,
        integration_id: &str,
        query: &str,
        page: u32,
        limit: u32,
    ) -> Result<crate::providers::internet_archive::ArchivePage, SearchError> {
        valid_query(query)?;
        let page = SearchPage {
            number: page,
            size: limit,
        };
        page.offset()?;
        let (adapter, scope) = self.archive_adapter(integration_id).await?;
        reserve_cooldown(&self.store, &scope, 1).await?;
        provider_result(&self.store, &scope, adapter.search(query, page).await).await
    }

    pub async fn archive_item(
        &self,
        integration_id: &str,
        identifier: &str,
    ) -> Result<crate::providers::internet_archive::ArchiveItem, SearchError> {
        if !crate::providers::internet_archive::valid_identifier(identifier) {
            return Err(SearchError::Invalid);
        }
        let (adapter, scope) = self.archive_adapter(integration_id).await?;
        reserve_cooldown(&self.store, &scope, 1).await?;
        provider_result(&self.store, &scope, adapter.item(identifier).await).await
    }

    async fn archive_adapter(
        &self,
        integration_id: &str,
    ) -> Result<(crate::providers::internet_archive::InternetArchive, String), SearchError> {
        let mut private = self.settings.load_private(integration_id).await?;
        if private.integration.kind != IntegrationKind::InternetArchive {
            return Err(SearchError::Unsupported);
        }
        if !private.integration.enabled {
            return Err(SettingsError::NotConfigured.into());
        }
        let mut base = reqwest::Url::parse(&private.integration.base_url)
            .map_err(|_| ProviderError::InvalidConfiguration)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        private.integration.base_url = base.to_string();
        let scope = credential_scope(&private.integration, None);
        let config = crate::providers::ProviderConfig::new(
            &private.integration.base_url,
            None,
            crate::providers::HttpLimits::default(),
        )?;
        Ok((
            crate::providers::internet_archive::InternetArchive::new(config)?,
            scope,
        ))
    }

    pub async fn releases(
        &self,
        owner: &str,
        request: ReleaseSearch,
    ) -> Result<Releases, SearchError> {
        valid_query(&request.query)?;
        if request.limit == 0
            || request.limit > 100
            || request.offset.checked_add(request.limit).is_none()
        {
            return Err(SearchError::Invalid);
        }
        let target = match request.unit_id.as_deref() {
            Some(id) => Some(
                WantedRepository::new(self.store.clone())
                    .unit_context(id)
                    .await
                    .map_err(|_| SearchError::Database)?
                    .ok_or(SearchError::UnitNotFound)?,
            ),
            None => None,
        };
        let content_type = target
            .as_ref()
            .map(|target| target.publication.content_type.clone())
            .unwrap_or_else(|| request.content_type.clone());
        if content_type != request.content_type {
            return Err(SearchError::TargetChanged);
        }
        if target.is_some() {
            SelectionRepository::new(self.store.clone())
                .cleanup_expired(100)
                .await
                .map_err(selection_error)?;
        }
        let policy = match target.as_ref() {
            Some(target) => Some(
                SelectionRepository::new(self.store.clone())
                    .current_or_default_policy(owner, &target.unit.id)
                    .await
                    .map_err(selection_error)?,
            ),
            None => None,
        };
        let private = self.settings.load_private(&request.integration_id).await?;
        let fingerprint = source_fingerprint(&private.integration)?;
        let scope = credential_scope(&private.integration, private.api_key.as_deref());
        let adapter = prowlarr(private)?;
        reserve_cooldown(&self.store, &scope, 1).await?;
        let result = adapter
            .search_offset(
                &request.query,
                content_type.clone(),
                request.offset,
                request.limit,
            )
            .await;
        let page = provider_result(&self.store, &scope, result).await?;
        let mut tx = self.store.begin_write().await?;
        // Expired handles referenced by durable intents remain recoverable.
        sqlx::query("DELETE FROM search_releases WHERE expires_at <= unixepoch() AND handle NOT IN (SELECT release_handle FROM acquisition_runs)").execute(&mut *tx).await?;
        let mut releases = Vec::with_capacity(page.releases.len());
        for release in page.releases {
            let handle = Uuid::new_v4().to_string();
            let guid_digest = digest(release.guid.as_bytes());
            let expires_at = now() + 3600;
            let evidence = parse_release_evidence(&release.title, content_type.clone());
            let evidence_json =
                serde_json::to_string(&evidence).map_err(|_| SearchError::Database)?;
            let handle: String = sqlx::query_scalar("INSERT INTO search_releases (handle, owner, integration_id, source_fingerprint, indexer_id, guid_digest, content_type, protocol, query, search_offset, search_limit, expires_at, evidence_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(owner, integration_id, source_fingerprint, indexer_id, guid_digest, content_type) DO UPDATE SET query = excluded.query, search_offset = excluded.search_offset, search_limit = excluded.search_limit, expires_at = excluded.expires_at, evidence_json = excluded.evidence_json RETURNING handle")
                .bind(handle).bind(owner).bind(&request.integration_id).bind(&fingerprint).bind(release.indexer_id)
                .bind(&guid_digest).bind(content_type.as_str()).bind(protocol_name(release.protocol))
                .bind(&request.query).bind(request.offset).bind(request.limit).bind(expires_at).bind(evidence_json).fetch_one(&mut *tx).await?;
            let evaluation = target.as_ref().map(|target| {
                crate::search::matching::evaluate(
                    target,
                    &crate::search::matching::Candidate {
                        source: &request.integration_id,
                        content_type: Evidence::Unknown,
                        evidence: &evidence,
                    },
                )
            });
            let (assessment_id, assessment_expires_at, active_rejection) =
                match (&target, &policy, &evaluation) {
                    (Some(target), Some(policy), Some(evaluation)) => {
                        let assessment = SelectionRepository::record_assessment_in(
                            &mut tx,
                            owner,
                            AssessmentInput {
                                target,
                                policy_id: &policy.id,
                                identity: ReleaseIdentity {
                                    handle: handle.clone(),
                                    integration_id: request.integration_id.clone(),
                                    source_fingerprint: fingerprint.clone(),
                                    indexer_id: release.indexer_id,
                                    guid_digest: guid_digest.clone(),
                                    content_type: content_type.clone(),
                                    protocol: release.protocol,
                                },
                                evidence: &evidence,
                                evaluation,
                                expires_at,
                            },
                        )
                        .await
                        .map_err(selection_error)?;
                        let rejection =
                            SelectionRepository::active_rejection_in(&mut tx, owner, &assessment)
                                .await
                                .map_err(selection_error)?;
                        (Some(assessment.id), Some(assessment.expires_at), rejection)
                    }
                    _ => (None, None, None),
                };
            releases.push(Release {
                release_handle: handle,
                integration_id: request.integration_id.clone(),
                indexer_id: release.indexer_id,
                title: display_title(&release.title),
                content_type: content_type.clone(),
                categories: release.categories,
                size_bytes: release.size_bytes,
                protocol: release.protocol,
                evidence,
                evaluation,
                assessment_id,
                assessment_expires_at,
                active_rejection,
            });
        }
        tx.commit().await?;
        Ok(Releases {
            releases,
            offset: page.offset,
            total: page.total,
            next_offset: page.next_offset,
            target,
        })
    }

    pub async fn assess_release(
        &self,
        owner: &str,
        request: ReleaseAssessmentRequest,
    ) -> Result<ReleaseAssessmentResponse, SearchError> {
        if !valid_uuid(&request.release_handle) || !valid_uuid(&request.unit_id) {
            return Err(SearchError::Invalid);
        }
        let source = sqlx::query(
            "SELECT integration_id, source_fingerprint FROM search_releases WHERE handle = ? AND owner = ? AND expires_at > unixepoch()",
        )
        .bind(&request.release_handle)
        .bind(owner)
        .fetch_optional(self.store.reader())
        .await?
        .ok_or(SearchError::NotFound)?;
        let source_fingerprint_at_search: String = source.try_get("source_fingerprint")?;
        let source: String = source.try_get("integration_id")?;
        let private = self.settings.load_private(&source).await?;
        let fingerprint = source_fingerprint(&private.integration)?;
        if fingerprint != source_fingerprint_at_search {
            return Err(SearchError::Changed);
        }
        prowlarr(private)?;
        let target = WantedRepository::new(self.store.clone())
            .unit_context(&request.unit_id)
            .await
            .map_err(|_| SearchError::Database)?
            .ok_or(SearchError::UnitNotFound)?;
        let policy = SelectionRepository::new(self.store.clone())
            .current_or_default_policy(owner, &request.unit_id)
            .await
            .map_err(selection_error)?;
        let mut tx = self.store.begin_write().await?;
        let row = sqlx::query("SELECT * FROM search_releases WHERE handle = ? AND owner = ? AND integration_id = ? AND source_fingerprint = ? AND expires_at > unixepoch()")
            .bind(&request.release_handle)
            .bind(owner)
            .bind(&source)
            .bind(&fingerprint)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(SearchError::NotFound)?;
        let content_type =
            ContentType::parse(row.try_get("content_type")?).map_err(|_| SearchError::Database)?;
        if content_type != target.publication.content_type {
            return Err(SearchError::TargetChanged);
        }
        let evidence_json: Option<String> = row.try_get("evidence_json")?;
        let evidence: ReleaseEvidence =
            serde_json::from_str(evidence_json.as_deref().ok_or(SearchError::NotFound)?)
                .map_err(|_| SearchError::Database)?;
        let protocol = match row.try_get::<&str, _>("protocol")? {
            "usenet" => ReleaseProtocol::Usenet,
            "torrent" => ReleaseProtocol::Torrent,
            _ => return Err(SearchError::Database),
        };
        let evaluation = crate::search::matching::evaluate(
            &target,
            &crate::search::matching::Candidate {
                source: &source,
                content_type: Evidence::Unknown,
                evidence: &evidence,
            },
        );
        let expires_at: i64 = row.try_get("expires_at")?;
        let assessment = SelectionRepository::record_assessment_in(
            &mut tx,
            owner,
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: ReleaseIdentity {
                    handle: request.release_handle,
                    integration_id: source,
                    source_fingerprint: fingerprint,
                    indexer_id: u32::try_from(row.try_get::<i64, _>("indexer_id")?)
                        .map_err(|_| SearchError::Database)?,
                    guid_digest: row.try_get("guid_digest")?,
                    content_type,
                    protocol,
                },
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at,
            },
        )
        .await
        .map_err(|error| assessment_error(error, expires_at))?;
        let active_rejection =
            SelectionRepository::active_rejection_in(&mut tx, owner, &assessment)
                .await
                .map_err(selection_error)?;
        tx.commit().await?;
        Ok(ReleaseAssessmentResponse {
            assessment_id: assessment.id,
            assessment_expires_at: assessment.expires_at,
            evaluation,
            active_rejection,
            target,
        })
    }
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

pub(super) fn selection_error(error: SelectionError) -> SearchError {
    match error {
        SelectionError::Changed => SearchError::TargetChanged,
        SelectionError::Invalid => SearchError::Invalid,
        SelectionError::NotFound => SearchError::UnitNotFound,
        SelectionError::Expired => SearchError::NotFound,
        SelectionError::Rejected | SelectionError::Superseded => SearchError::ReleaseRejected,
        SelectionError::Database
        | SelectionError::Conflict
        | SelectionError::Ineligible
        | SelectionError::AcknowledgementRequired => SearchError::Database,
    }
}

/// The release handle expiry is the assessment window; it can close mid-request.
pub(super) fn assessment_error(error: SelectionError, expires_at: i64) -> SearchError {
    match error {
        SelectionError::Invalid if expires_at <= now() => SearchError::NotFound,
        error => selection_error(error),
    }
}

/// Only called for a durable explicitly selected intent. No descriptor or payload is persisted.
pub(crate) async fn selected_payload(
    store: &SqliteStore,
    settings: &Settings,
    handle: &str,
) -> Result<(Vec<u8>, ReleaseProtocol, ReleaseEvidence), SearchError> {
    let row = sqlx::query("SELECT * FROM search_releases WHERE handle = ?")
        .bind(handle)
        .fetch_optional(store.reader())
        .await?
        .ok_or(SearchError::NotFound)?;
    let integration_id: String = row.try_get("integration_id")?;
    let private = settings.load_private(&integration_id).await?;
    if source_fingerprint(&private.integration)?
        != row.try_get::<String, _>("source_fingerprint")?
    {
        return Err(SearchError::Changed);
    }
    let scope = credential_scope(&private.integration, private.api_key.as_deref());
    let adapter = prowlarr(private)?;
    reserve_cooldown(store, &scope, 1).await?;
    let content_type =
        ContentType::parse(row.try_get("content_type")?).map_err(|_| SearchError::Database)?;
    let query: String = row.try_get("query")?;
    let result = adapter
        .search_for_acquisition(
            &query,
            content_type.clone(),
            row.try_get("search_offset")?,
            row.try_get("search_limit")?,
        )
        .await;
    let page = provider_result(store, &scope, result).await?;
    let guid: String = row.try_get("guid_digest")?;
    let indexer_id: u32 = row.try_get("indexer_id")?;
    let selected = page
        .releases
        .into_iter()
        .find(|r| r.release.indexer_id == indexer_id && digest(r.release.guid.as_bytes()) == guid)
        .ok_or(SearchError::NotFound)?;
    if protocol_name(selected.release.protocol) != row.try_get::<String, _>("protocol")? {
        return Err(SearchError::Changed);
    }
    let evidence = parse_release_evidence(&selected.release.title, content_type);
    let descriptor = selected.download.ok_or(SearchError::Unsupported)?;
    let result = adapter.retrieve_payload(&descriptor).await;
    Ok((
        provider_result(store, &scope, result).await?,
        selected.release.protocol,
        evidence,
    ))
}

pub(crate) fn source_fingerprint(integration: &Integration) -> Result<String, SearchError> {
    // Secret rotation is allowed. Host, kind, category, mapping and source changes are not.
    let value = serde_json::to_vec(&serde_json::json!([
        integration.id,
        integration.kind,
        integration.base_url,
        integration.options
    ]))
    .map_err(|_| SearchError::Database)?;
    Ok(digest(&value))
}
fn prowlarr(private: PrivateIntegration) -> Result<crate::providers::Prowlarr, SearchError> {
    let integration = private.integration;
    if !integration.enabled || !integration.credentials_configured {
        return Err(SettingsError::NotConfigured.into());
    }
    if integration.kind != IntegrationKind::Prowlarr {
        return Err(SearchError::Unsupported);
    }
    let IntegrationOptions::Prowlarr(options) = integration.options else {
        return Err(SearchError::Unsupported);
    };
    let config = crate::providers::ProviderConfig::new(
        &integration.base_url,
        private.api_key,
        crate::providers::HttpLimits::default(),
    )?;
    Ok(crate::providers::Prowlarr::new(
        config,
        options.indexer_id,
        match options.protocol {
            Protocol::Usenet => ReleaseProtocol::Usenet,
            Protocol::Torrent => ReleaseProtocol::Torrent,
        },
        crate::providers::CategoryMap {
            comics: options.categories.comics,
            manga: options.categories.manga,
            magazines: options.categories.magazines,
        },
    )?)
}
fn credential_scope(integration: &Integration, key: Option<&str>) -> String {
    digest(
        serde_json::json!([integration.kind, integration.base_url, key])
            .to_string()
            .as_bytes(),
    )
}
pub(crate) async fn reserve_cooldown(
    store: &SqliteStore,
    scope: &str,
    seconds: i64,
) -> Result<(), SearchError> {
    let mut tx = store.begin_write().await?;
    let next: Option<i64> =
        sqlx::query_scalar("SELECT next_at FROM search_cooldowns WHERE scope = ?")
            .bind(scope)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(next) = next
        && next > now()
    {
        return Err(SearchError::Cooldown {
            retry_after_seconds: (next - now()) as u64,
        });
    }
    sqlx::query("INSERT INTO search_cooldowns (scope, next_at) VALUES (?, ?) ON CONFLICT(scope) DO UPDATE SET next_at = excluded.next_at").bind(scope).bind(now() + seconds).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn provider_result<T>(
    store: &SqliteStore,
    scope: &str,
    result: Result<T, ProviderError>,
) -> Result<T, SearchError> {
    if let Err(ProviderError::RateLimited {
        retry_after_seconds,
    }) = &result
    {
        let wait = retry_after_seconds
            .unwrap_or(60)
            .min(i64::MAX as u64 - now() as u64) as i64;
        let mut tx = store.begin_write().await?;
        sqlx::query("INSERT INTO search_cooldowns (scope, next_at) VALUES (?, ?) ON CONFLICT(scope) DO UPDATE SET next_at = MAX(next_at, excluded.next_at)").bind(scope).bind(now() + wait).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    result.map_err(Into::into)
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) fn protocol_name(protocol: ReleaseProtocol) -> &'static str {
    match protocol {
        ReleaseProtocol::Torrent => "torrent",
        ReleaseProtocol::Usenet => "usenet",
    }
}
fn valid_query(query: &str) -> Result<(), SearchError> {
    if query.trim().is_empty()
        || query.len() > 512
        || query.chars().any(char::is_control)
        || query.contains("://")
    {
        Err(SearchError::Invalid)
    } else {
        Ok(())
    }
}
fn display_title(title: &str) -> String {
    if !safe_release_title(title) {
        "Release title unavailable".into()
    } else {
        title.chars().take(512).collect()
    }
}
