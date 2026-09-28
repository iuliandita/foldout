//! Selected direct downloads and complete MangaDex chapters. No durable URLs, automatic resubmission, or resume.
use crate::{
    importer::{
        finalize,
        journal::{ImportError, ImportPhase, ImportPolicy, ImportService, InternalImportRequest},
    },
    providers::{
        HttpLimits, ProviderConfig, ProviderError, SearchPage,
        getcomics::*,
        mangadex_chapters::{
            MangaDexChapter, MangaDexChapters, MangaDexManifest, MangaDexScanlationGroup,
        },
    },
    settings::{IntegrationKind, Settings, SettingsError},
    store::sqlite::SqliteStore,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::{IpAddr, SocketAddr},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TRANSFER_TIME: Duration = Duration::from_secs(15 * 60);
static SLOT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum DirectError {
    #[error("invalid direct acquisition request")]
    Invalid,
    #[error("direct selection or acquisition was not found")]
    NotFound,
    #[error("selection, unit, destination, or idempotency key conflicts")]
    Conflict,
    #[error("direct worker is busy")]
    Busy,
    #[error("intent requires import or local review before cancellation")]
    ReviewRequired,
    #[error("source changed; select the link again")]
    Changed,
    #[error("direct source requires manual action")]
    ManualAction,
    #[error("download destination is not an allowed public host")]
    NetworkPolicy,
    #[error("download failed or was interrupted; review required")]
    Transfer,
    #[error("download exceeds the size limit")]
    SizeLimit,
    #[error("download archive validation failed")]
    InvalidArchive,
    #[error("registered root or file needs local permission or identity review")]
    LocalReview,
    #[error("direct acquisition database operation failed")]
    Database,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}
impl From<sqlx::Error> for DirectError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error()
            .is_some_and(|e| e.is_unique_violation() || e.code().as_deref() == Some("1811"))
        {
            Self::Conflict
        } else {
            Self::Database
        }
    }
}
#[derive(Clone)]
pub struct Direct {
    store: SqliteStore,
    #[cfg(test)]
    fixture: Option<SocketAddr>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectSearch {
    pub integration_id: String,
    pub query: String,
    #[serde(default = "first")]
    pub page: u32,
}
fn first() -> u32 {
    1
}
#[derive(Serialize)]
pub struct DirectPost {
    pub post_handle: String,
    pub title: String,
}
#[derive(Serialize)]
pub struct DirectSearchPage {
    pub posts: Vec<DirectPost>,
    pub page: u32,
    pub next_page: Option<u32>,
}
#[derive(Serialize)]
pub struct DirectLink {
    pub link_handle: String,
    #[serde(flatten)]
    pub summary: GetComicsLinkSummary,
}
#[derive(Serialize)]
pub struct DirectDetail {
    pub post_handle: String,
    pub title: String,
    pub links: Vec<DirectLink>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectChapterSearch {
    pub integration_id: String,
    pub manga_id: String,
    pub language: String,
    #[serde(default = "first")]
    pub page: u32,
    #[serde(default = "chapter_limit")]
    pub limit: u32,
}
fn chapter_limit() -> u32 {
    20
}
#[derive(Serialize)]
pub struct DirectChapter {
    pub link_handle: String,
    pub chapter_id: String,
    pub manga_id: String,
    pub language: String,
    pub chapter: Option<String>,
    pub volume: Option<String>,
    pub title: Option<String>,
    pub scanlation_groups: Vec<MangaDexScanlationGroup>,
    pub page_count: u32,
    pub version: u32,
    pub state: GetComicsLinkState,
}
#[derive(Serialize)]
pub struct DirectChapterPage {
    pub chapters: Vec<DirectChapter>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
    pub next_page: Option<u32>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectDestination {
    pub root_id: String,
    pub relative_path: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRequest {
    pub link_handle: String,
    pub unit_id: String,
    pub download_root_id: String,
    pub destination: DirectDestination,
}
#[derive(Debug, Serialize)]
pub struct DirectAcquisition {
    pub id: String,
    pub unit_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub attempted: bool,
    pub downloaded_bytes: i64,
    pub import_id: String,
    pub updated_at: i64,
    pub canceled_at: Option<i64>,
}
struct Selection {
    source_kind: String,
    integration_id: String,
    fingerprint: String,
    post_path: String,
    digest: String,
}

impl Direct {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            #[cfg(test)]
            fixture: None,
        }
    }

    pub async fn chapters(
        &self,
        settings: &Settings,
        owner: &str,
        input: DirectChapterSearch,
    ) -> Result<DirectChapterPage, DirectError> {
        label(owner)?;
        let (provider, fingerprint) = manga_adapter(settings, &input.integration_id).await?;
        let page = provider
            .list_chapters(
                &input.manga_id,
                &input.language,
                SearchPage {
                    number: input.page,
                    size: input.limit,
                },
            )
            .await?;
        let mut chapters = Vec::new();
        for chapter in page.chapters {
            let digest = chapter_identity(&chapter)?;
            let mut tx = self.store.begin_write().await?;
            let handle: String = sqlx::query_scalar("INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path,link_digest,source_kind) VALUES(?,?,?,?,'',?,'mangadex_chapter') ON CONFLICT(owner,integration_id,source_fingerprint,post_path,link_digest) DO UPDATE SET post_path=excluded.post_path RETURNING handle")
                .bind(Uuid::new_v4().to_string()).bind(owner).bind(&input.integration_id).bind(&fingerprint).bind(&digest).fetch_one(&mut *tx).await?;
            sqlx::query("INSERT INTO direct_chapter_selections(handle,chapter_id,manga_id,language,metadata_identity,page_count) VALUES(?,?,?,?,?,?) ON CONFLICT(handle) DO NOTHING")
                .bind(&handle).bind(&chapter.id).bind(&chapter.manga_id).bind(&chapter.language).bind(&digest).bind(chapter.page_count).execute(&mut *tx).await?;
            tx.commit().await?;
            let state = if chapter.external_url.is_some() {
                GetComicsLinkState::ManualAction
            } else if chapter.is_unavailable || chapter.page_count == 0 {
                GetComicsLinkState::Unsupported
            } else {
                GetComicsLinkState::Direct
            };
            chapters.push(DirectChapter {
                link_handle: handle,
                chapter_id: chapter.id,
                manga_id: chapter.manga_id,
                language: chapter.language,
                chapter: chapter.chapter,
                volume: chapter.volume,
                title: chapter.title,
                scanlation_groups: chapter.scanlation_groups,
                page_count: chapter.page_count,
                version: chapter.version,
                state,
            });
        }
        Ok(DirectChapterPage {
            chapters,
            total: page.total,
            page: input.page,
            page_size: page.page_size,
            next_page: page.next_page,
        })
    }

    async fn selected_chapter(
        &self,
        settings: &Settings,
        owner: &str,
        handle: &str,
    ) -> Result<(MangaDexChapters, MangaDexChapter), DirectError> {
        let selection = self.selection(owner, handle).await?;
        if selection.source_kind != "mangadex_chapter" {
            return Err(DirectError::Invalid);
        }
        let (provider, fingerprint) = manga_adapter(settings, &selection.integration_id).await?;
        if fingerprint != selection.fingerprint {
            return Err(DirectError::Changed);
        }
        let row = sqlx::query("SELECT * FROM direct_chapter_selections WHERE handle=?")
            .bind(handle)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(DirectError::NotFound)?;
        let chapter = provider
            .get_chapter(&row.try_get::<String, _>("chapter_id")?)
            .await?;
        if chapter_identity(&chapter)? != selection.digest
            || selection.digest != row.try_get::<String, _>("metadata_identity")?
            || chapter.manga_id != row.try_get::<String, _>("manga_id")?
            || chapter.language != row.try_get::<String, _>("language")?
            || chapter.page_count != row.try_get::<u32, _>("page_count")?
        {
            return Err(DirectError::Changed);
        }
        if chapter.external_url.is_some() {
            return Err(DirectError::ManualAction);
        }
        if chapter.is_unavailable || chapter.page_count == 0 || chapter.page_count > 1000 {
            return Err(ProviderError::Unsupported.into());
        }
        Ok((provider, chapter))
    }

    pub async fn search(
        &self,
        settings: &Settings,
        owner: &str,
        input: DirectSearch,
    ) -> Result<DirectSearchPage, DirectError> {
        label(owner)?;
        let (adapter, fingerprint) = adapter(settings, &input.integration_id).await?;
        let page = adapter.search(&input.query, input.page).await?;
        let mut posts = Vec::new();
        for post in page.posts {
            let handle = self
                .save_selection(
                    owner,
                    &input.integration_id,
                    &fingerprint,
                    &post.post_path,
                    "",
                )
                .await?;
            posts.push(DirectPost {
                post_handle: handle,
                title: post.title,
            });
        }
        Ok(DirectSearchPage {
            posts,
            page: page.page,
            next_page: page.next_page,
        })
    }

    pub async fn detail(
        &self,
        settings: &Settings,
        owner: &str,
        handle: &str,
    ) -> Result<DirectDetail, DirectError> {
        let selected = self.selection(owner, handle).await?;
        if selected.source_kind != "getcomics" || !selected.digest.is_empty() {
            return Err(DirectError::Invalid);
        }
        let adapter = checked_adapter(settings, &selected).await?;
        let detail = adapter.detail(&selected.post_path).await?;
        let public = detail.summary();
        let mut links = Vec::new();
        for (link, summary) in detail.links().iter().zip(public.links) {
            let handle = self
                .save_selection(
                    owner,
                    &selected.integration_id,
                    &selected.fingerprint,
                    &selected.post_path,
                    &link.identity_digest(),
                )
                .await?;
            links.push(DirectLink {
                link_handle: handle,
                summary,
            });
        }
        Ok(DirectDetail {
            post_handle: handle.to_owned(),
            title: public.post.title,
            links,
        })
    }

    pub async fn resolve(
        &self,
        settings: &Settings,
        owner: &str,
        handle: &str,
    ) -> Result<GetComicsLinkSummary, DirectError> {
        Ok(self.selected(settings, owner, handle).await?.summary)
    }

    async fn selected(
        &self,
        settings: &Settings,
        owner: &str,
        handle: &str,
    ) -> Result<GetComicsResolution, DirectError> {
        let selected = self.selection(owner, handle).await?;
        if selected.source_kind != "getcomics" || selected.digest.is_empty() {
            return Err(DirectError::Invalid);
        }
        let adapter = checked_adapter(settings, &selected).await?;
        let detail = adapter.detail(&selected.post_path).await?;
        let link = detail
            .links()
            .iter()
            .find(|l| l.identity_digest() == selected.digest)
            .ok_or(DirectError::Changed)?;
        Ok(adapter.resolve(link).await?)
    }

    async fn selection(&self, owner: &str, handle: &str) -> Result<Selection, DirectError> {
        id(handle)?;
        let row = sqlx::query("SELECT * FROM direct_selections WHERE handle = ? AND owner = ?")
            .bind(handle)
            .bind(owner)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(DirectError::NotFound)?;
        Ok(Selection {
            source_kind: row.try_get("source_kind")?,
            integration_id: row.try_get("integration_id")?,
            fingerprint: row.try_get("source_fingerprint")?,
            post_path: row.try_get("post_path")?,
            digest: row.try_get("link_digest")?,
        })
    }
    async fn save_selection(
        &self,
        owner: &str,
        integration: &str,
        fingerprint: &str,
        post: &str,
        digest: &str,
    ) -> Result<String, DirectError> {
        let mut tx = self.store.begin_write().await?;
        let handle = sqlx::query_scalar("INSERT INTO direct_selections(handle, owner, integration_id, source_fingerprint, post_path, link_digest) VALUES(?,?,?,?,?,?) ON CONFLICT(owner,integration_id,source_fingerprint,post_path,link_digest) DO UPDATE SET post_path=excluded.post_path RETURNING handle")
            .bind(Uuid::new_v4().to_string()).bind(owner).bind(integration).bind(fingerprint).bind(post).bind(digest).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(handle)
    }

    pub async fn create(
        &self,
        settings: &Settings,
        owner: &str,
        key: &str,
        request: DirectRequest,
    ) -> Result<DirectAcquisition, DirectError> {
        label(owner)?;
        label(key)?;
        for value in [
            &request.link_handle,
            &request.unit_id,
            &request.download_root_id,
            &request.destination.root_id,
        ] {
            id(value)?;
        }
        destination_relative(&request.destination.relative_path)?;
        let digest = hash(&serde_json::to_vec(&request).map_err(|_| DirectError::Invalid)?);
        if let Some(existing) = self.replay(owner, key, &digest).await? {
            return Ok(existing);
        }
        // Validate the source selection before reserving an intent; never persist a resolved URL.
        let selected = self.selection(owner, &request.link_handle).await?;
        let language = if selected.source_kind == "mangadex_chapter" {
            let (_, chapter) = self
                .selected_chapter(settings, owner, &request.link_handle)
                .await?;
            if Path::new(&request.destination.relative_path)
                .extension()
                .and_then(|s| s.to_str())
                != Some("cbz")
            {
                return Err(DirectError::Invalid);
            }
            Some(chapter.language)
        } else {
            let outcome = self.selected(settings, owner, &request.link_handle).await?;
            match outcome.summary.state {
                GetComicsLinkState::Direct => {}
                GetComicsLinkState::ManualAction => return Err(DirectError::ManualAction),
                _ => return Err(ProviderError::Unsupported.into()),
            }
            None
        };
        let mut tx = self.store.begin_write().await?;
        if let Some(row) = sqlx::query("SELECT id, request_fingerprint FROM direct_acquisitions WHERE owner=? AND idempotency_key=?")
            .bind(owner).bind(key).fetch_optional(&mut *tx).await? {
            if row.try_get::<String,_>("request_fingerprint")? != digest { return Err(DirectError::Conflict); }
            let existing: String = row.try_get("id")?;
            tx.commit().await?;
            return self.get(owner, &existing).await;
        }
        check_unit(&mut tx, &request.unit_id, language.as_deref()).await?;
        for root in [&request.download_root_id, &request.destination.root_id] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_roots WHERE id=?)")
                    .bind(root)
                    .fetch_one(&mut *tx)
                    .await?;
            if !exists {
                return Err(DirectError::NotFound);
            }
        }
        let reserved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM file_coverage WHERE unit_id=?) OR EXISTS(SELECT 1 FROM import_operations WHERE unit_id=? OR (destination_root=? AND destination_relative=?))")
            .bind(&request.unit_id).bind(&request.unit_id).bind(&request.destination.root_id).bind(&request.destination.relative_path).fetch_one(&mut *tx).await?;
        if reserved {
            return Err(DirectError::Conflict);
        }
        let acquisition_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state) VALUES(?,?,?,?,?,?,?,?,?,?,'queued')")
            .bind(&acquisition_id).bind(owner).bind(key).bind(digest).bind(&request.link_handle).bind(&request.unit_id).bind(&request.download_root_id).bind(&request.destination.root_id).bind(&request.destination.relative_path).bind(Uuid::new_v4().to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(owner, &acquisition_id).await
    }
    /// Release an idle pre-import reservation without touching files or transfer evidence.
    pub async fn cancel(
        &self,
        owner: &str,
        acquisition_id: &str,
    ) -> Result<DirectAcquisition, DirectError> {
        label(owner)?;
        id(acquisition_id)?;
        let existing = self.get(owner, acquisition_id).await?;
        if existing.state == "canceled" {
            return Ok(existing);
        }
        let _slot = SLOT
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .try_lock()
            .map_err(|_| DirectError::Busy)?;
        let _process_lock = self.worker_lock().await?.ok_or(DirectError::Busy)?;
        let mut tx = self.store.begin_write().await?;
        let row = sqlx::query("SELECT * FROM direct_acquisitions WHERE id=? AND owner=?")
            .bind(acquisition_id)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DirectError::NotFound)?;
        if row.try_get::<&str, _>("state")? == "canceled" {
            return view(row);
        }
        if !matches!(
            row.try_get::<&str, _>("state")?,
            "queued" | "needs_review" | "downloaded"
        ) {
            return Err(DirectError::ReviewRequired);
        }
        // Any journal can own work beyond the caller's lifetime, even in planned phase.
        let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM import_operations WHERE id=? OR unit_id=? OR (destination_root=? AND destination_relative=?)) OR EXISTS(SELECT 1 FROM file_coverage WHERE unit_id=?)")
            .bind(row.try_get::<String,_>("import_id")?).bind(row.try_get::<String,_>("unit_id")?)
            .bind(row.try_get::<String,_>("destination_root_id")?).bind(row.try_get::<String,_>("destination_relative")?)
            .bind(row.try_get::<String,_>("unit_id")?).fetch_one(&mut *tx).await?;
        if blocked {
            return Err(DirectError::ReviewRequired);
        }
        let changed = sqlx::query("UPDATE direct_acquisitions SET state='canceled',canceled_at=unixepoch(),updated_at=unixepoch() WHERE id=? AND owner=? AND state IN ('queued','needs_review','downloaded')")
            .bind(acquisition_id).bind(owner).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DirectError::Conflict);
        }
        tx.commit().await?;
        self.get(owner, acquisition_id).await
    }
    async fn replay(
        &self,
        owner: &str,
        key: &str,
        digest: &str,
    ) -> Result<Option<DirectAcquisition>, DirectError> {
        let row =
            sqlx::query("SELECT * FROM direct_acquisitions WHERE owner=? AND idempotency_key=?")
                .bind(owner)
                .bind(key)
                .fetch_optional(self.store.reader())
                .await?;
        row.map(|r| {
            if r.try_get::<String, _>("request_fingerprint")? != digest {
                Err(DirectError::Conflict)
            } else {
                view(r)
            }
        })
        .transpose()
    }
    pub async fn get(
        &self,
        owner: &str,
        acquisition_id: &str,
    ) -> Result<DirectAcquisition, DirectError> {
        let row = sqlx::query("SELECT * FROM direct_acquisitions WHERE id=? AND owner=?")
            .bind(acquisition_id)
            .bind(owner)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(DirectError::NotFound)?;
        view(row)
    }
    pub async fn list(
        &self,
        owner: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<DirectAcquisition>, DirectError> {
        if limit == 0 || limit > 100 {
            return Err(DirectError::Invalid);
        }
        sqlx::query("SELECT * FROM direct_acquisitions WHERE owner=? ORDER BY created_at DESC,id DESC LIMIT ? OFFSET ?").bind(owner).bind(limit).bind(offset).fetch_all(self.store.reader()).await?.into_iter().map(view).collect()
    }

    /// Process mutex plus database-directory flock serialize workers even across restarts.
    /// No transaction spans network or file work.
    pub async fn tick(&self, settings: Settings) -> Result<bool, DirectError> {
        let Ok(_slot) = SLOT.get_or_init(|| tokio::sync::Mutex::new(())).try_lock() else {
            return Ok(false);
        };
        let Some(_process_lock) = self.worker_lock().await? else {
            return Ok(false);
        };
        let token = Uuid::new_v4().to_string();
        let mut tx = self.store.begin_write().await?;
        let acquired = sqlx::query(
            "UPDATE direct_worker_lock SET token=?,until_at=unixepoch()+1200 WHERE id=1",
        )
        .bind(&token)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !acquired {
            tx.commit().await?;
            return Ok(false);
        }
        // Previous process could have issued GET. Never reset its durable fence.
        let interrupted = sqlx::query("UPDATE direct_acquisitions SET state='needs_review',reason='interrupted_transfer',updated_at=unixepoch() WHERE state='downloading'").execute(&mut *tx).await?.rows_affected();
        let row = sqlx::query("SELECT * FROM direct_acquisitions WHERE state IN ('queued','downloaded','importing') ORDER BY created_at,id LIMIT 1").fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let worked = row.is_some() || interrupted > 0;
        let result = if let Some(row) = row {
            let acquisition_id: String = row.try_get("id")?;
            let outcome =
                tokio::time::timeout(Duration::from_secs(18 * 60), self.process(&settings, &row))
                    .await
                    .unwrap_or(Err(DirectError::Transfer));
            match outcome {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.state(&acquisition_id, "needs_review", Some(reason(&error)))
                        .await
                }
            }
        } else {
            Ok(())
        };
        let mut tx = self.store.begin_write().await?;
        sqlx::query("UPDATE direct_worker_lock SET token=NULL,until_at=0 WHERE token=?")
            .bind(&token)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        result?;
        Ok(worked)
    }

    async fn process(
        &self,
        settings: &Settings,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<(), DirectError> {
        let acquisition_id: String = row.try_get("id")?;
        if row.try_get::<&str, _>("state")? != "queued" {
            return self.import_step(row).await;
        }
        if row.try_get::<bool, _>("attempted")? {
            return Err(DirectError::Transfer);
        }
        let owner: String = row.try_get("owner")?;
        let handle: String = row.try_get("link_handle")?;
        let selection = self.selection(&owner, &handle).await?;
        let (download, manifest, language) = if selection.source_kind == "mangadex_chapter" {
            let (provider, chapter) = self.selected_chapter(settings, &owner, &handle).await?;
            let manifest = provider.at_home(&chapter).await?;
            if manifest.summary().chapter_id != chapter.id
                || manifest.summary().page_count != chapter.page_count
            {
                return Err(DirectError::Changed);
            }
            (None, Some(manifest), Some(chapter.language))
        } else {
            let resolution = self.selected(settings, &owner, &handle).await?;
            let download = match resolution.summary.state {
                GetComicsLinkState::Direct => {
                    resolution.into_download().ok_or(DirectError::Changed)?
                }
                GetComicsLinkState::ManualAction => return Err(DirectError::ManualAction),
                _ => return Err(ProviderError::Unsupported.into()),
            };
            (Some(download), None, None)
        };
        let source = self
            .root(&row.try_get::<String, _>("download_root_id")?)
            .await?;
        let destination = self
            .root(&row.try_get::<String, _>("destination_root_id")?)
            .await?;
        let destination_relative: String = row.try_get("destination_relative")?;
        let format = match &download {
            Some(download) => archive_format(download.url())?,
            None => "cbz",
        };
        if Path::new(&destination_relative)
            .extension()
            .and_then(|s| s.to_str())
            != Some(format)
        {
            return Err(DirectError::InvalidArchive);
        }
        let source_path = source.clone();
        let dest_path = destination.clone();
        let dest_relative = destination_relative.clone();
        let stage_id = acquisition_id.clone();
        let (stage, dest_identity) = tokio::task::spawn_blocking(move || {
            let dest = finalize::root(&dest_path).map_err(|_| DirectError::LocalReview)?;
            absent_destination(&dest, &dest_relative)?;
            Ok::<_, DirectError>((Stage::create(&source_path, &stage_id)?, identity(&dest)?))
        })
        .await
        .map_err(|_| DirectError::LocalReview)??;
        let root_identity = identity(&stage.root)?;
        // Commit before DNS/GET. Cancellation, process death, or write failure leaves review state.
        let mut tx = self.store.begin_write().await?;
        check_unit(
            &mut tx,
            &row.try_get::<String, _>("unit_id")?,
            language.as_deref(),
        )
        .await?;
        let fenced = sqlx::query("UPDATE direct_acquisitions SET attempted=1,state='downloading',root_identity=?,destination_identity=?,manifest_identity=?,expected_pages=?,updated_at=unixepoch() WHERE id=? AND attempted=0 AND state='queued' AND manifest_identity IS NULL AND expected_pages IS NULL")
            .bind(&root_identity).bind(dest_identity).bind(manifest.as_ref().map(|m| m.identity())).bind(manifest.as_ref().map(|m| m.summary().page_count)).bind(&acquisition_id).execute(&mut *tx).await?.rows_affected();
        if fenced != 1 {
            return Err(DirectError::Conflict);
        }
        tx.commit().await?;
        let expected_pages = manifest.as_ref().map(|m| m.summary().page_count);
        let (size, digest, source_identity) = tokio::time::timeout(TRANSFER_TIME, async {
            if let Some(manifest) = &manifest {
                self.transfer_chapter(manifest, &stage.file, &stage.job)
                    .await?;
                if stage
                    .file
                    .metadata()
                    .map_err(|_| DirectError::LocalReview)?
                    .len()
                    > 512 * 1024 * 1024
                {
                    return Err(DirectError::SizeLimit);
                }
            } else {
                let download = download.as_ref().ok_or(DirectError::Changed)?;
                let (client, url) = self.client(download.url()).await?;
                transfer(client, url, &stage.file, MAX_BYTES).await?;
            }
            let validated = fingerprint_file(&stage.file).await?;
            verify_archive_count(&stage.file, format, expected_pages).await?;
            stage.check(&source)?;
            let (size, digest) = fingerprint_file(&stage.file).await?;
            if validated != (size, digest.clone()) {
                return Err(DirectError::LocalReview);
            }
            stage.publish(format)?;
            Ok::<_, DirectError>((size, digest, identity(&stage.file)?))
        })
        .await
        .map_err(|_| DirectError::Transfer)??;
        let relative = format!(".library-downloads/{acquisition_id}/file.{format}");
        let mut tx = self.store.begin_write().await?;
        sqlx::query("UPDATE direct_acquisitions SET state='downloaded',downloaded_bytes=?,content_digest=?,source_identity=?,source_relative=?,updated_at=unixepoch() WHERE id=? AND state='downloading'")
            .bind(size as i64).bind(digest).bind(source_identity).bind(relative).bind(&acquisition_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn import_step(&self, row: &sqlx::sqlite::SqliteRow) -> Result<(), DirectError> {
        let acquisition_id: String = row.try_get("id")?;
        let selection = self
            .selection(
                &row.try_get::<String, _>("owner")?,
                &row.try_get::<String, _>("link_handle")?,
            )
            .await?;
        let language: Option<String> = if selection.source_kind == "mangadex_chapter" {
            Some(
                sqlx::query_scalar("SELECT language FROM direct_chapter_selections WHERE handle=?")
                    .bind(row.try_get::<String, _>("link_handle")?)
                    .fetch_one(self.store.reader())
                    .await?,
            )
        } else {
            None
        };
        let mut tx = self.store.begin_write().await?;
        check_unit(
            &mut tx,
            &row.try_get::<String, _>("unit_id")?,
            language.as_deref(),
        )
        .await?;
        tx.commit().await?;
        let source_root: String = row.try_get("download_root_id")?;
        let destination_root: String = row.try_get("destination_root_id")?;
        let source = self.root(&source_root).await?;
        let destination = self.root(&destination_root).await?;
        let source_relative: String = row.try_get("source_relative")?;
        let source_dir = finalize::root(&source).map_err(|_| DirectError::LocalReview)?;
        let dest_dir = finalize::root(&destination).map_err(|_| DirectError::LocalReview)?;
        if identity(&source_dir)? != row.try_get::<String, _>("root_identity")?
            || identity(&dest_dir)? != row.try_get::<String, _>("destination_identity")?
        {
            return Err(DirectError::LocalReview);
        }
        let file = open_relative(&source_dir, &source_relative)?;
        if identity(&file)? != row.try_get::<String, _>("source_identity")? {
            return Err(DirectError::LocalReview);
        }
        let (size, digest) = fingerprint_file(&file).await?;
        if digest != row.try_get::<String, _>("content_digest")?
            || size as i64 != row.try_get::<i64, _>("downloaded_bytes")?
        {
            return Err(DirectError::LocalReview);
        }
        let service = ImportService::new(self.store.clone());
        let import_id: String = row.try_get("import_id")?;
        let operation = service
            .plan(
                &import_id,
                InternalImportRequest {
                    source_root,
                    source_relative,
                    destination_root,
                    destination_relative: row.try_get("destination_relative")?,
                    unit_id: row.try_get("unit_id")?,
                    policy: ImportPolicy::Copy,
                },
            )
            .await
            .map_err(import_error)?;
        // Tie the journal's captured inode/content to the downloaded evidence before advancing.
        if operation.identity.signature != digest
            || operation.identity.size != size as i64
            || format!(
                "{}:{}",
                operation.identity.source.dev, operation.identity.source.ino
            ) != row.try_get::<String, _>("source_identity")?
        {
            return Err(DirectError::LocalReview);
        }
        let operation = service.step(&import_id).await.map_err(import_error)?;
        self.state(
            &acquisition_id,
            if operation.phase == ImportPhase::Done {
                "completed"
            } else {
                "importing"
            },
            None,
        )
        .await
    }
    async fn root(&self, id: &str) -> Result<PathBuf, DirectError> {
        let path: String = sqlx::query_scalar("SELECT path FROM library_roots WHERE id=?")
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(DirectError::LocalReview)?;
        Ok(path.into())
    }
    async fn state(&self, id: &str, state: &str, reason: Option<&str>) -> Result<(), DirectError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query(
            "UPDATE direct_acquisitions SET state=?,reason=?,updated_at=unixepoch() WHERE id=?",
        )
        .bind(state)
        .bind(reason)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn worker_lock(&self) -> Result<Option<File>, DirectError> {
        let databases = sqlx::query("PRAGMA database_list")
            .fetch_all(self.store.reader())
            .await?;
        let database = databases
            .iter()
            .find(|row| row.get::<&str, _>("name") == "main")
            .ok_or(DirectError::Database)?;
        let path = PathBuf::from(database.try_get::<String, _>("file")?);
        let root = finalize::root(path.parent().ok_or(DirectError::LocalReview)?)
            .map_err(|_| DirectError::LocalReview)?;
        let lock = open_at(&root, ".direct-worker.lock", libc::O_RDWR | libc::O_CREAT)?;
        let metadata = lock.metadata().map_err(|_| DirectError::LocalReview)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(DirectError::LocalReview);
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(DirectError::LocalReview);
        }
        Ok(Some(lock))
    }
    async fn transfer_chapter(
        &self,
        manifest: &MangaDexManifest,
        file: &File,
        job: &File,
    ) -> Result<(), DirectError> {
        #[cfg(test)]
        if let Some(address) = self.fixture {
            return super::manga_transfer::transfer_fixture(manifest, file, job, address)
                .await
                .map_err(manga_transfer_error);
        }
        super::manga_transfer::transfer(manifest, file, job)
            .await
            .map_err(manga_transfer_error)
    }
    async fn client(
        &self,
        url: &reqwest::Url,
    ) -> Result<(reqwest::Client, reqwest::Url), DirectError> {
        archive_format(url)?;
        #[cfg(test)]
        if let Some(address) = self.fixture {
            let mut target = reqwest::Url::parse(&format!("http://{address}/"))
                .map_err(|_| DirectError::Invalid)?;
            target.set_path(url.path());
            target.set_query(url.query());
            return Ok((
                client_builder()
                    .build()
                    .map_err(|_| DirectError::NetworkPolicy)?,
                target,
            ));
        }
        let host = url.host_str().ok_or(DirectError::NetworkPolicy)?;
        let addresses: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((host, 443)),
        )
        .await
        .map_err(|_| DirectError::NetworkPolicy)?
        .map_err(|_| DirectError::NetworkPolicy)?
        .take(17)
        .collect();
        if addresses.is_empty()
            || addresses.len() > 16
            || addresses.iter().any(|a| !public_ip(a.ip()))
        {
            return Err(DirectError::NetworkPolicy);
        }
        let client = client_builder()
            .https_only(true)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| DirectError::NetworkPolicy)?;
        Ok((client, url.clone()))
    }
}

fn manga_transfer_error(error: super::manga_transfer::MangaTransferError) -> DirectError {
    use super::manga_transfer::MangaTransferError as E;
    match error {
        E::Changed => DirectError::Changed,
        E::NetworkPolicy => DirectError::NetworkPolicy,
        E::SizeLimit => DirectError::SizeLimit,
        E::InvalidImage | E::Package => DirectError::InvalidArchive,
        E::LocalReview => DirectError::LocalReview,
        _ => DirectError::Transfer,
    }
}
async fn check_unit(
    connection: &mut sqlx::SqliteConnection,
    unit: &str,
    language: Option<&str>,
) -> Result<(), DirectError> {
    let row = sqlx::query("SELECT p.content_type,e.language FROM units u JOIN editions e ON e.id=u.edition_id JOIN publications p ON p.id=e.publication_id WHERE u.id=?").bind(unit).fetch_optional(connection).await?.ok_or(DirectError::NotFound)?;
    let expected = if language.is_some() { "manga" } else { "comic" };
    if row.try_get::<&str, _>("content_type")? != expected
        || language.is_some_and(|l| row.get::<&str, _>("language") != l)
    {
        return Err(DirectError::Invalid);
    }
    Ok(())
}
fn chapter_identity(chapter: &MangaDexChapter) -> Result<String, DirectError> {
    let mut groups = chapter.scanlation_groups.clone();
    groups.sort_by(|a, b| a.id.cmp(&b.id));
    // External URLs never enter durable state, even for manual selections.
    let metadata = serde_json::json!([
        chapter.id,
        chapter.manga_id,
        chapter.language,
        chapter.chapter,
        chapter.volume,
        chapter.title,
        groups,
        chapter.page_count,
        chapter.version,
        chapter.is_unavailable,
        chapter.external_url.is_some()
    ]);
    Ok(hash(
        &serde_json::to_vec(&metadata).map_err(|_| DirectError::Invalid)?,
    ))
}
async fn manga_adapter(
    settings: &Settings,
    integration_id: &str,
) -> Result<(MangaDexChapters, String), DirectError> {
    id(integration_id)?;
    let config = settings.get(integration_id).await?;
    if config.kind != IntegrationKind::MangaDex || !config.enabled {
        return Err(SettingsError::NotConfigured.into());
    }
    let fingerprint =
        crate::search::source_fingerprint(&config).map_err(|_| DirectError::Database)?;
    Ok((
        MangaDexChapters::new(ProviderConfig::new(
            &config.base_url,
            None,
            HttpLimits::default(),
        )?)?,
        fingerprint,
    ))
}
async fn adapter(
    settings: &Settings,
    integration_id: &str,
) -> Result<(GetComicsAdapter, String), DirectError> {
    id(integration_id)?;
    let config = settings.get(integration_id).await?;
    if config.kind != IntegrationKind::GetComics || !config.enabled {
        return Err(SettingsError::NotConfigured.into());
    }
    let fingerprint =
        crate::search::source_fingerprint(&config).map_err(|_| DirectError::Database)?;
    let provider = GetComicsAdapter::new(ProviderConfig::new(
        &config.base_url,
        None,
        HttpLimits::default(),
    )?)?;
    Ok((provider, fingerprint))
}
async fn checked_adapter(
    settings: &Settings,
    selected: &Selection,
) -> Result<GetComicsAdapter, DirectError> {
    let (provider, fingerprint) = adapter(settings, &selected.integration_id).await?;
    if fingerprint != selected.fingerprint {
        return Err(DirectError::Changed);
    }
    Ok(provider)
}
fn view(row: sqlx::sqlite::SqliteRow) -> Result<DirectAcquisition, DirectError> {
    Ok(DirectAcquisition {
        id: row.try_get("id")?,
        unit_id: row.try_get("unit_id")?,
        state: row.try_get("state")?,
        reason: row.try_get("reason")?,
        attempted: row.try_get("attempted")?,
        downloaded_bytes: row.try_get("downloaded_bytes")?,
        import_id: row.try_get("import_id")?,
        updated_at: row.try_get("updated_at")?,
        canceled_at: row.try_get("canceled_at")?,
    })
}
fn id(value: &str) -> Result<(), DirectError> {
    if Uuid::parse_str(value).is_ok_and(|u| u.to_string() == value) {
        Ok(())
    } else {
        Err(DirectError::Invalid)
    }
}
fn label(value: &str) -> Result<(), DirectError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(DirectError::Invalid)
    } else {
        Ok(())
    }
}
fn hash(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn destination_relative(value: &str) -> Result<(), DirectError> {
    finalize::relative(value).map_err(|_| DirectError::Invalid)?;
    if value.len() > 1024
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value.split('/').any(|v| v == ".library-downloads")
        || !matches!(
            Path::new(value).extension().and_then(|e| e.to_str()),
            Some("cbz" | "cbr")
        )
    {
        return Err(DirectError::Invalid);
    }
    Ok(())
}
fn archive_format(url: &reqwest::Url) -> Result<&'static str, DirectError> {
    if url.scheme() != "https"
        || !url.host_str().is_some_and(direct_host_allowed)
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 8192
    {
        return Err(DirectError::NetworkPolicy);
    }
    match Path::new(url.path())
        .extension()
        .and_then(|v| v.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("cbz" | "zip") => Ok("cbz"),
        Some("cbr" | "rar") => Ok("cbr"),
        _ => Err(DirectError::InvalidArchive),
    }
}
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent("libraryd/0.1")
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .timeout(TRANSFER_TIME)
}
/// Conservative public-unicast policy: reject transition, documentation, and special ranges.
pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}
async fn transfer(
    client: reqwest::Client,
    url: reqwest::Url,
    file: &File,
    limit: u64,
) -> Result<(), DirectError> {
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| DirectError::Transfer)?;
    if response
        .headers()
        .get("cf-mitigated")
        .is_some_and(|v| v == "challenge")
    {
        return Err(ProviderError::ChallengeRequired.into());
    }
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::RateLimited {
            retry_after_seconds: None,
        }
        .into());
    }
    // A 206 without an authorized validator is not a complete archive response.
    if response.status() != reqwest::StatusCode::OK {
        return Err(DirectError::Transfer);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return Err(DirectError::InvalidArchive);
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "application/octet-stream"
            | "application/zip"
            | "application/x-zip-compressed"
            | "application/x-cbz"
            | "application/vnd.comicbook+zip"
            | "application/x-rar-compressed"
            | "application/vnd.rar"
            | "application/x-cbr"
            | "application/vnd.comicbook-rar"
    ) {
        return Err(DirectError::InvalidArchive);
    }
    let length = response.content_length();
    if length.is_some_and(|n| n == 0 || n > limit) {
        return Err(DirectError::SizeLimit);
    }
    let mut output =
        tokio::fs::File::from_std(file.try_clone().map_err(|_| DirectError::LocalReview)?);
    let mut bytes = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|_| DirectError::Transfer)? {
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .ok_or(DirectError::SizeLimit)?;
        if bytes > limit {
            return Err(DirectError::SizeLimit);
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|_| DirectError::LocalReview)?;
    }
    if bytes == 0 || length.is_some_and(|n| n != bytes) {
        return Err(DirectError::Transfer);
    }
    output.flush().await.map_err(|_| DirectError::LocalReview)?;
    output
        .sync_all()
        .await
        .map_err(|_| DirectError::LocalReview)?;
    Ok(())
}
async fn verify_archive_count(
    file: &File,
    format: &str,
    expected: Option<u32>,
) -> Result<(), DirectError> {
    let mut descriptor = file.try_clone().map_err(|_| DirectError::LocalReview)?;
    descriptor
        .seek(SeekFrom::Start(0))
        .map_err(|_| DirectError::LocalReview)?;
    let mut magic = [0u8; 8];
    descriptor
        .read_exact(&mut magic)
        .map_err(|_| DirectError::InvalidArchive)?;
    if !(match format {
        "cbz" => magic.starts_with(b"PK\x03\x04"),
        "cbr" => magic.starts_with(b"Rar!\x1a\x07\x00") || magic == *b"Rar!\x1a\x07\x01\x00",
        _ => false,
    }) {
        return Err(DirectError::InvalidArchive);
    }
    let decoder = crate::reader::archive::ArchiveDecoder::new();
    let path = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        descriptor.as_raw_fd()
    ));
    let manifest = decoder
        .manifest(&path)
        .await
        .map_err(|_| DirectError::InvalidArchive)?;
    if expected.is_some_and(|count| manifest.pages.len() != count as usize) {
        return Err(DirectError::InvalidArchive);
    }
    for page in manifest.pages {
        decoder
            .page(&path, &page.name)
            .await
            .map_err(|_| DirectError::InvalidArchive)?;
    }
    Ok(())
}
async fn fingerprint_file(file: &File) -> Result<(u64, String), DirectError> {
    let mut descriptor = file.try_clone().map_err(|_| DirectError::LocalReview)?;
    tokio::task::spawn_blocking(move || {
        let before = descriptor
            .metadata()
            .map_err(|_| DirectError::LocalReview)?;
        if !before.is_file() || before.len() == 0 {
            return Err(DirectError::LocalReview);
        }
        if before.len() > MAX_BYTES {
            return Err(DirectError::SizeLimit);
        }
        descriptor
            .seek(SeekFrom::Start(0))
            .map_err(|_| DirectError::LocalReview)?;
        // The read itself is capped, including growth after the metadata check.
        let mut limited = (&mut descriptor).take(MAX_BYTES + 1);
        let mut buffer = [0u8; 65536];
        let mut size = 0u64;
        let mut digest = Sha256::new();
        loop {
            let count = limited
                .read(&mut buffer)
                .map_err(|_| DirectError::LocalReview)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if size > MAX_BYTES {
                return Err(DirectError::SizeLimit);
            }
            digest.update(&buffer[..count]);
        }
        let after = descriptor
            .metadata()
            .map_err(|_| DirectError::LocalReview)?;
        if size != before.len()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(DirectError::LocalReview);
        }
        Ok((
            size,
            digest
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        ))
    })
    .await
    .map_err(|_| DirectError::LocalReview)?
}
fn import_error(error: ImportError) -> DirectError {
    match error {
        ImportError::InvalidFormat => DirectError::InvalidArchive,
        _ => DirectError::LocalReview,
    }
}
fn reason(error: &DirectError) -> &'static str {
    match error {
        DirectError::Provider(ProviderError::ChallengeRequired) => "challenge_required",
        DirectError::Provider(ProviderError::Unsupported) => "unsupported",
        DirectError::Provider(ProviderError::RateLimited { .. }) => "rate_limited",
        DirectError::ManualAction => "manual_action",
        DirectError::Changed => "source_changed",
        DirectError::Settings(_) => "configuration_changed",
        DirectError::LocalReview => "local_review",
        DirectError::NetworkPolicy => "network_policy",
        DirectError::SizeLimit => "size_limit",
        DirectError::InvalidArchive => "invalid_archive",
        DirectError::Transfer => "interrupted_transfer",
        _ => "source_unavailable",
    }
}
fn identity(file: &File) -> Result<String, DirectError> {
    let m = file.metadata().map_err(|_| DirectError::LocalReview)?;
    Ok(format!("{}:{}", m.dev(), m.ino()))
}
fn cstr(s: &str) -> Result<CString, DirectError> {
    CString::new(s).map_err(|_| DirectError::LocalReview)
}
fn open_at(parent: &File, name: &str, flags: i32) -> Result<File, DirectError> {
    let name = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(DirectError::LocalReview);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn private_dir(parent: &File, name: &str, exclusive: bool) -> Result<File, DirectError> {
    let c = cstr(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o700) } != 0
        && (exclusive || std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST))
    {
        return Err(DirectError::LocalReview);
    }
    let directory = open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let metadata = directory.metadata().map_err(|_| DirectError::LocalReview)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err(DirectError::LocalReview);
    }
    parent.sync_all().map_err(|_| DirectError::LocalReview)?;
    Ok(directory)
}
fn parent_at(root: &File, relative: &str) -> Result<(File, String), DirectError> {
    finalize::relative(relative).map_err(|_| DirectError::LocalReview)?;
    let mut parts: Vec<_> = relative.split('/').collect();
    let name = parts.pop().ok_or(DirectError::LocalReview)?.to_owned();
    let mut parent = root.try_clone().map_err(|_| DirectError::LocalReview)?;
    for part in parts {
        parent = open_at(&parent, part, libc::O_RDONLY | libc::O_DIRECTORY)?;
    }
    Ok((parent, name))
}
fn open_relative(root: &File, relative: &str) -> Result<File, DirectError> {
    let (parent, name) = parent_at(root, relative)?;
    let file = open_at(&parent, &name, libc::O_RDONLY)?;
    if !file
        .metadata()
        .map_err(|_| DirectError::LocalReview)?
        .is_file()
    {
        return Err(DirectError::LocalReview);
    }
    Ok(file)
}
fn absent_destination(root: &File, relative: &str) -> Result<(), DirectError> {
    let (parent, name) = parent_at(root, relative)?;
    let name = cstr(&name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
    {
        return Err(DirectError::LocalReview);
    }
    Ok(())
}
struct Stage {
    root: File,
    area: File,
    job: File,
    file: File,
    id: String,
}
impl Stage {
    fn create(path: &Path, id: &str) -> Result<Self, DirectError> {
        let root = finalize::root(path).map_err(|_| DirectError::LocalReview)?;
        let area = private_dir(&root, ".library-downloads", false)?;
        let job = private_dir(&area, id, true)?;
        let file = open_at(
            &job,
            "filepartial",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        let m = file.metadata().map_err(|_| DirectError::LocalReview)?;
        if !m.is_file()
            || m.nlink() != 1
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o777 != 0o600
        {
            return Err(DirectError::LocalReview);
        }
        job.sync_all().map_err(|_| DirectError::LocalReview)?;
        Ok(Self {
            root,
            area,
            job,
            file,
            id: id.to_owned(),
        })
    }
    fn check(&self, path: &Path) -> Result<(), DirectError> {
        for (fd, expected) in [
            (&self.root, path.to_path_buf()),
            (&self.area, path.join(".library-downloads")),
            (&self.job, path.join(".library-downloads").join(&self.id)),
        ] {
            let actual = std::fs::canonicalize(format!("/proc/self/fd/{}", fd.as_raw_fd()))
                .map_err(|_| DirectError::LocalReview)?;
            if actual != expected {
                return Err(DirectError::LocalReview);
            }
        }
        if identity(&open_at(&self.job, "filepartial", libc::O_RDONLY)?)? != identity(&self.file)? {
            return Err(DirectError::LocalReview);
        }
        Ok(())
    }
    fn publish(&self, format: &str) -> Result<(), DirectError> {
        let from = cstr("filepartial")?;
        let to = cstr(&format!("file.{format}"))?;
        if unsafe {
            // musl may lack the wrapper symbol; call the same Linux operation directly.
            libc::syscall(
                libc::SYS_renameat2,
                self.job.as_raw_fd(),
                from.as_ptr(),
                self.job.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(DirectError::LocalReview);
        }
        self.job.sync_all().map_err(|_| DirectError::LocalReview)
    }
}

#[cfg(test)]
#[path = "direct_test.rs"]
mod tests;
