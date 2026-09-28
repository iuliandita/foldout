//! Durable per-unit, per-source searches. Results always require explicit selection.
//!
//! Fixed intervals (900..=31536000 seconds) are UTC epoch-aligned, not cron/IANA
//! schedules. Creation schedules the next boundary; run requests make work due now.
//! Missed intervals coalesce into one search. A 45-second bounded search holds a
//! 120-second lease; restart recovers abandoned leases after expiry. No DB write
//! transaction spans network I/O. Handles expire through the shared search service.
use crate::{
    catalog::{
        ContentType,
        wanted::{UnitContext, context_row},
    },
    providers::ProviderError,
    search::{ReleaseSearch, Releases, Search, SearchError},
    settings::{IntegrationKind, IntegrationOptions, Settings, SettingsError},
    store::sqlite::SqliteStore,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};
use uuid::Uuid;

const CANDIDATE_LIMIT: u32 = 20;
const LEASE_SECONDS: i64 = 120;
const SOURCE_SNAPSHOT: &str =
    "SELECT json_array(kind, base_url, enabled, options) FROM integrations WHERE id = ?";
macro_rules! monitor_target_query {
    ($suffix:literal $(,)?) => {
        concat!(
            r#"
WITH monitor_target AS (
    SELECT m.*, i.label AS integration_label, m.owner AS monitor_owner, m.unit_id AS monitor_unit_id, m.integration_id AS monitor_integration_id,
        p.id AS p_id, p.content_type AS p_content_type, p.title AS p_title, p.sort_title AS p_sort_title, p.run_label AS p_run_label, p.title_locked AS p_title_locked, p.known_unit_count AS p_known_unit_count,
        e.id AS e_id, e.publication_id AS e_publication_id, e.language AS e_language, e.region AS e_region, e.publisher AS e_publisher,
        u.id AS u_id, u.edition_id AS u_edition_id, u.label AS u_label, u.kind AS u_kind, u.sort_key AS u_sort_key, u.date AS u_date, u.date_precision AS u_date_precision
    FROM monitors m JOIN integrations i ON i.id = m.integration_id JOIN units u ON u.id = m.unit_id JOIN editions e ON e.id = u.edition_id JOIN publications p ON p.id = e.publication_id
)
SELECT * FROM monitor_target
"#,
            $suffix
        )
    };
}

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("Invalid monitor or pagination")]
    Invalid,
    #[error("Monitor or scope not found")]
    NotFound,
    #[error("Monitor scope or revision conflicts")]
    Conflict,
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("Monitor database operation failed")]
    Database,
}
impl From<sqlx::Error> for MonitorError {
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionPolicy {
    ReviewOnly,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMonitor {
    pub unit_id: String,
    pub integration_id: String,
    pub query: String,
    pub interval_seconds: i64,
    pub enabled: bool,
    /// Required even though review_only is currently the sole supported policy.
    pub selection_policy: SelectionPolicy,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMonitor {
    pub revision: i64,
    #[serde(default, deserialize_with = "non_null")]
    pub query: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    pub interval_seconds: Option<i64>,
    #[serde(default, deserialize_with = "non_null")]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "non_null")]
    pub selection_policy: Option<SelectionPolicy>,
}
fn non_null<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, Serialize)]
pub struct MonitorCandidate {
    pub release_handle: String,
    pub integration_id: String,
    pub indexer_id: u32,
    pub title: String,
    pub content_type: ContentType,
    pub categories: Vec<u32>,
    pub size_bytes: u64,
    pub protocol: String,
    pub expires_at: i64,
}
#[derive(Serialize)]
pub struct MonitorView {
    pub id: String,
    pub unit_id: String,
    pub target: UnitContext,
    pub integration_id: String,
    pub integration_label: String,
    pub content_type: ContentType,
    pub query: String,
    pub interval_seconds: i64,
    pub timezone: &'static str,
    pub selection_policy: SelectionPolicy,
    pub enabled: bool,
    pub revision: i64,
    pub next_run: i64,
    pub last_run_at: Option<i64>,
    pub last_state: String,
    pub reason: Option<String>,
    pub running: bool,
    pub candidates: Vec<MonitorCandidate>,
    pub candidates_truncated: bool,
}
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorFilters {
    pub publication_id: Option<String>,
    pub unit_id: Option<String>,
    pub enabled: Option<bool>,
}
#[derive(Deserialize, Serialize)]
struct MonitorCursor {
    owner: String,
    filters: MonitorFilters,
    last_id: String,
}
#[derive(Serialize)]
pub struct MonitorPage {
    pub items: Vec<MonitorView>,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct Monitor {
    store: SqliteStore,
    settings: Settings,
    search: Search,
}
struct Claim {
    item: MonitorView,
    owner: String,
    token: String,
    source_snapshot: String,
}

impl Monitor {
    pub fn new(store: SqliteStore, settings: Settings) -> Self {
        Self {
            search: Search::new(store.clone(), settings.clone()),
            store,
            settings,
        }
    }

    pub async fn create(
        &self,
        owner: &str,
        request: CreateMonitor,
    ) -> Result<MonitorView, MonitorError> {
        validate(&request.query, request.interval_seconds)?;
        let content_type = self.content_type(&request.unit_id).await?;
        let source = self.settings.get(&request.integration_id).await?;
        let IntegrationOptions::Prowlarr(options) = source.options else {
            return Err(MonitorError::Invalid);
        };
        let categories = match content_type {
            ContentType::Comic => options.categories.comics,
            ContentType::Manga => options.categories.manga,
            ContentType::Magazine => options.categories.magazines,
        };
        if source.kind != IntegrationKind::Prowlarr || categories.is_empty() {
            return Err(MonitorError::Invalid);
        }
        if !source.enabled || !source.credentials_configured {
            return Err(SettingsError::NotConfigured.into());
        }
        let id = Uuid::new_v4().to_string();
        let mut tx = self.store.begin_write().await?;
        sqlx::query("INSERT INTO monitors (id, owner, unit_id, integration_id, content_type, query, interval_seconds, selection_policy, enabled, next_run, last_state) VALUES (?, ?, ?, ?, ?, ?, ?, 'review_only', ?, ?, ?)")
            .bind(&id).bind(owner).bind(&request.unit_id).bind(&request.integration_id).bind(content_type.as_str()).bind(request.query.trim()).bind(request.interval_seconds).bind(request.enabled)
            .bind(next_boundary(now(), request.interval_seconds)).bind(if request.enabled { "scheduled" } else { "disabled" }).execute(&mut *tx).await?;
        let row = sqlx::query(monitor_target_query!("WHERE id = ?"))
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        let view = decode(&row)?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn get(&self, owner: &str, id: &str) -> Result<MonitorView, MonitorError> {
        let row = sqlx::query(monitor_target_query!(
            "WHERE owner = ? AND id = ? AND deleted_at IS NULL",
        ))
        .bind(owner)
        .bind(id)
        .fetch_optional(self.store.reader())
        .await?
        .ok_or(MonitorError::NotFound)?;
        decode(&row)
    }

    pub async fn list(
        &self,
        owner: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MonitorPage, MonitorError> {
        self.list_filtered(owner, limit, cursor, MonitorFilters::default())
            .await
    }

    pub async fn list_filtered(
        &self,
        owner: &str,
        limit: u32,
        cursor: Option<&str>,
        filters: MonitorFilters,
    ) -> Result<MonitorPage, MonitorError> {
        if !(1..=100).contains(&limit) {
            return Err(MonitorError::Invalid);
        }
        let legacy_cursor = cursor.is_some_and(|value| Uuid::parse_str(value).is_ok());
        let last_id = decode_cursor(owner, &filters, cursor)?;
        if legacy_cursor
            && !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM monitors WHERE owner = ? AND id = ?)",
            )
            .bind(owner)
            .bind(&last_id)
            .fetch_one(self.store.reader())
            .await?
        {
            return Err(MonitorError::Invalid);
        }
        let rows = sqlx::query(monitor_target_query!("WHERE owner = ? AND deleted_at IS NULL AND (? IS NULL OR e_publication_id = ?) AND (? IS NULL OR unit_id = ?) AND (? IS NULL OR enabled = ?) AND id > ? ORDER BY id LIMIT ?"))
            .bind(owner).bind(filters.publication_id.as_deref()).bind(filters.publication_id.as_deref())
            .bind(filters.unit_id.as_deref()).bind(filters.unit_id.as_deref()).bind(filters.enabled).bind(filters.enabled)
            .bind(last_id).bind(limit + 1).fetch_all(self.store.reader()).await?;
        let has_more = rows.len() > limit as usize;
        let items = rows
            .iter()
            .take(limit as usize)
            .map(decode)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| encode_cursor(owner, &filters, &item.id))
        } else {
            None
        };
        Ok(MonitorPage { items, next_cursor })
    }

    pub async fn update(
        &self,
        owner: &str,
        id: &str,
        request: UpdateMonitor,
    ) -> Result<MonitorView, MonitorError> {
        let mut tx = self.store.begin_write().await?;
        let row = owned_revision(&mut tx, owner, id, request.revision).await?;
        let old = decode(&row)?;
        let query = request.query.unwrap_or(old.query);
        let interval = request.interval_seconds.unwrap_or(old.interval_seconds);
        let enabled = request.enabled.unwrap_or(old.enabled);
        validate(&query, interval)?;
        // Keep the old lease: changing a revision invalidates publication, not in-flight I/O.
        sqlx::query("UPDATE monitors SET query = ?, interval_seconds = ?, enabled = ?, revision = revision + 1, next_run = ?, last_state = ?, reason = NULL, candidates = '[]', candidates_truncated = 0, updated_at = unixepoch() WHERE id = ?")
            .bind(query.trim()).bind(interval).bind(enabled).bind(next_boundary(now(),interval)).bind(if enabled {"scheduled"} else {"disabled"}).bind(id).execute(&mut *tx).await?;
        let row = sqlx::query(monitor_target_query!("WHERE id = ?"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let view = decode(&row)?;
        tx.commit().await?;
        Ok(view)
    }

    /// A local schedule mutation only; it never calls a provider or download client.
    pub async fn run(
        &self,
        owner: &str,
        id: &str,
        revision: i64,
    ) -> Result<MonitorView, MonitorError> {
        let mut tx = self.store.begin_write().await?;
        let row = owned_revision(&mut tx, owner, id, revision).await?;
        if !row.try_get::<bool, _>("enabled")? {
            return Err(MonitorError::Conflict);
        }
        sqlx::query("UPDATE monitors SET next_run = unixepoch(), revision = revision + 1, updated_at = unixepoch() WHERE id = ?")
            .bind(id).execute(&mut *tx).await?;
        let row = sqlx::query(monitor_target_query!("WHERE id = ?"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let view = decode(&row)?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn delete(&self, owner: &str, id: &str, revision: i64) -> Result<(), MonitorError> {
        let mut tx = self.store.begin_write().await?;
        owned_revision(&mut tx, owner, id, revision).await?;
        sqlx::query("UPDATE monitors SET enabled = 0, revision = revision + 1, deleted_at = unixepoch(), last_state = 'canceled', reason = NULL, candidates = '[]', candidates_truncated = 0, updated_at = unixepoch() WHERE id = ?")
            .bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns true when one due run was attempted, including fenced or failed searches.
    pub async fn tick(&self) -> Result<bool, MonitorError> {
        let Some(claim) = self.claim().await? else {
            return Ok(false);
        };
        let search = async {
            if self
                .content_type(&claim.item.unit_id)
                .await
                .map_err(|_| SearchError::Database)?
                != claim.item.content_type
            {
                return Err(SearchError::Changed);
            }
            self.search
                .releases(
                    &claim.owner,
                    ReleaseSearch {
                        integration_id: claim.item.integration_id.clone(),
                        content_type: claim.item.content_type.clone(),
                        query: claim.item.query.clone(),
                        unit_id: Some(claim.item.unit_id.clone()),
                        offset: 0,
                        limit: CANDIDATE_LIMIT,
                    },
                )
                .await
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(45), search)
            .await
            .unwrap_or(Err(SearchError::Provider(ProviderError::Unavailable)));
        self.finish(&claim, result).await?;
        Ok(true)
    }

    async fn content_type(&self, unit: &str) -> Result<ContentType, MonitorError> {
        let value: String = sqlx::query_scalar("SELECT p.content_type FROM units u JOIN editions e ON e.id = u.edition_id JOIN publications p ON p.id = e.publication_id WHERE u.id = ?")
            .bind(unit).fetch_optional(self.store.reader()).await?.ok_or(MonitorError::NotFound)?;
        ContentType::parse(value).map_err(|_| MonitorError::Database)
    }

    async fn claim(&self) -> Result<Option<Claim>, MonitorError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query("DELETE FROM monitors WHERE deleted_at IS NOT NULL AND (lease_until IS NULL OR lease_until <= unixepoch())").execute(&mut *tx).await?;
        let row = sqlx::query(monitor_target_query!("WHERE enabled = 1 AND deleted_at IS NULL AND next_run <= unixepoch() AND (lease_until IS NULL OR lease_until <= unixepoch()) AND NOT EXISTS (SELECT 1 FROM monitors other WHERE other.owner = monitor_owner AND other.unit_id = monitor_unit_id AND other.integration_id = monitor_integration_id AND other.lease_until > unixepoch()) ORDER BY next_run, id LIMIT 1"))
            .fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let item = decode(&row)?;
        let owner = row.try_get("owner")?;
        let source_snapshot = sqlx::query_scalar(SOURCE_SNAPSHOT)
            .bind(&item.integration_id)
            .fetch_one(&mut *tx)
            .await?;
        let token = Uuid::new_v4().to_string();
        sqlx::query(
            "UPDATE monitors SET lease_token = ?, lease_until = unixepoch() + ? WHERE id = ?",
        )
        .bind(&token)
        .bind(LEASE_SECONDS)
        .bind(&item.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(Claim {
            item,
            owner,
            token,
            source_snapshot,
        }))
    }

    async fn finish(
        &self,
        claim: &Claim,
        result: Result<Releases, SearchError>,
    ) -> Result<bool, MonitorError> {
        let mut tx = self.store.begin_write().await?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM monitors WHERE id = ? AND revision = ? AND enabled = 1 AND deleted_at IS NULL AND lease_token = ? AND lease_until > unixepoch())")
            .bind(&claim.item.id).bind(claim.item.revision).bind(&claim.token).fetch_one(&mut *tx).await?;
        if !valid {
            sqlx::query("UPDATE monitors SET lease_token = NULL, lease_until = NULL WHERE id = ? AND lease_token = ?").bind(&claim.item.id).bind(&claim.token).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let snapshot: String = sqlx::query_scalar(SOURCE_SNAPSHOT)
            .bind(&claim.item.integration_id)
            .fetch_one(&mut *tx)
            .await?;
        let result = if snapshot == claim.source_snapshot {
            result
        } else {
            Err(SearchError::Changed)
        };
        let mut candidates = Vec::new();
        let mut truncated = false;
        let mut next_run = next_boundary(now(), claim.item.interval_seconds);
        let (state, reason) = match result {
            Ok(page) => {
                let found = !page.releases.is_empty();
                truncated = page.next_offset.is_some()
                    || page.releases.len() > CANDIDATE_LIMIT as usize
                    || page.total.is_some_and(|n| n > page.releases.len() as u64);
                for release in page.releases.into_iter().take(CANDIDATE_LIMIT as usize) {
                    let expires: Option<i64> = sqlx::query_scalar("SELECT expires_at FROM search_releases WHERE handle = ? AND owner = ? AND integration_id = ? AND expires_at > unixepoch()")
                        .bind(&release.release_handle).bind(&claim.owner).bind(&claim.item.integration_id).fetch_optional(&mut *tx).await?;
                    if let Some(expires_at) = expires {
                        candidates.push(MonitorCandidate {
                            release_handle: release.release_handle,
                            integration_id: release.integration_id,
                            indexer_id: release.indexer_id,
                            title: release.title,
                            content_type: release.content_type,
                            categories: release.categories.into_iter().take(100).collect(),
                            size_bytes: release.size_bytes,
                            protocol: match release.protocol {
                                crate::providers::ReleaseProtocol::Usenet => "usenet",
                                crate::providers::ReleaseProtocol::Torrent => "torrent",
                            }
                            .into(),
                            expires_at,
                        });
                    }
                }
                // A bounded first page cannot establish absence when more results exist.
                if found || truncated {
                    ("needs_review", None)
                } else {
                    ("awaiting_release", None)
                }
            }
            Err(error) => {
                let reason = match error {
                    SearchError::Cooldown {
                        retry_after_seconds,
                    } => {
                        next_run = next_run.max(
                            now().saturating_add(retry_after_seconds.min(i64::MAX as u64) as i64),
                        );
                        "source_cooldown"
                    }
                    SearchError::Provider(ProviderError::RateLimited {
                        retry_after_seconds,
                    }) => {
                        next_run = next_run.max(now().saturating_add(
                            retry_after_seconds.unwrap_or(60).min(i64::MAX as u64) as i64,
                        ));
                        "source_cooldown"
                    }
                    SearchError::Changed => "source_changed",
                    SearchError::Settings(_) => "source_not_configured",
                    SearchError::Invalid | SearchError::Unsupported => "source_unsupported",
                    SearchError::Database => "search_storage_error",
                    _ => "source_unavailable",
                };
                ("source_error", Some(reason))
            }
        };
        if next_run > now() {
            next_run = next_boundary(next_run - 1, claim.item.interval_seconds);
        }
        let json = serde_json::to_string(&candidates).map_err(|_| MonitorError::Database)?;
        sqlx::query("UPDATE monitors SET revision = revision + 1, next_run = ?, last_run_at = unixepoch(), last_state = ?, reason = ?, candidates = ?, candidates_truncated = ?, lease_token = NULL, lease_until = NULL, updated_at = unixepoch() WHERE id = ? AND revision = ? AND lease_token = ?")
            .bind(next_run).bind(state).bind(reason).bind(json).bind(truncated).bind(&claim.item.id).bind(claim.item.revision).bind(&claim.token).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
}

async fn owned_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    id: &str,
    revision: i64,
) -> Result<SqliteRow, MonitorError> {
    if revision < 1 {
        return Err(MonitorError::Invalid);
    }
    let row = sqlx::query(monitor_target_query!(
        "WHERE owner = ? AND id = ? AND deleted_at IS NULL",
    ))
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(MonitorError::NotFound)?;
    if row.try_get::<i64, _>("revision")? != revision {
        return Err(MonitorError::Conflict);
    }
    Ok(row)
}
fn decode(row: &SqliteRow) -> Result<MonitorView, MonitorError> {
    let candidates: String = row.try_get("candidates")?;
    Ok(MonitorView {
        id: row.try_get("id")?,
        unit_id: row.try_get("unit_id")?,
        target: context_row(row).map_err(|_| MonitorError::Database)?,
        integration_id: row.try_get("integration_id")?,
        integration_label: row.try_get("integration_label")?,
        content_type: ContentType::parse(row.try_get("content_type")?)
            .map_err(|_| MonitorError::Database)?,
        query: row.try_get("query")?,
        interval_seconds: row.try_get("interval_seconds")?,
        timezone: "UTC",
        selection_policy: SelectionPolicy::ReviewOnly,
        enabled: row.try_get("enabled")?,
        revision: row.try_get("revision")?,
        next_run: row.try_get("next_run")?,
        last_run_at: row.try_get("last_run_at")?,
        last_state: row.try_get("last_state")?,
        reason: row.try_get("reason")?,
        running: row
            .try_get::<Option<i64>, _>("lease_until")?
            .is_some_and(|n| n > now()),
        candidates: serde_json::from_str(&candidates).map_err(|_| MonitorError::Database)?,
        candidates_truncated: row.try_get("candidates_truncated")?,
    })
}
fn encode_cursor(owner: &str, filters: &MonitorFilters, last_id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&MonitorCursor {
            owner: owner.into(),
            filters: filters.clone(),
            last_id: last_id.into(),
        })
        .expect("cursor is serializable"),
    )
}
fn decode_cursor(
    owner: &str,
    filters: &MonitorFilters,
    cursor: Option<&str>,
) -> Result<String, MonitorError> {
    let Some(cursor) = cursor else {
        return Ok(String::new());
    };
    if cursor.len() > 8 * 1024 {
        return Err(MonitorError::Invalid);
    }
    if Uuid::parse_str(cursor).is_ok() {
        return if filters.publication_id.is_none()
            && filters.unit_id.is_none()
            && filters.enabled.is_none()
        {
            Ok(cursor.into())
        } else {
            Err(MonitorError::Invalid)
        };
    }
    let cursor: MonitorCursor = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| MonitorError::Invalid)
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|_| MonitorError::Invalid))?;
    if cursor.owner != owner
        || cursor.filters.publication_id != filters.publication_id
        || cursor.filters.unit_id != filters.unit_id
        || cursor.filters.enabled != filters.enabled
        || Uuid::parse_str(&cursor.last_id).is_err()
    {
        return Err(MonitorError::Invalid);
    }
    Ok(cursor.last_id)
}
fn validate(query: &str, interval: i64) -> Result<(), MonitorError> {
    if query.trim().is_empty()
        || query.len() > 512
        || query.chars().any(char::is_control)
        || query.contains("://")
        || !(900..=31536000).contains(&interval)
    {
        return Err(MonitorError::Invalid);
    }
    Ok(())
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn next_boundary(now: i64, interval: i64) -> i64 {
    now.saturating_add(interval - now.rem_euclid(interval))
}

#[cfg(test)]
#[path = "monitor_test.rs"]
mod tests;
