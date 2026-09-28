use std::{
    collections::{HashMap, VecDeque},
    fs::{File, Metadata, OpenOptions},
    io::Cursor,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Weak},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use super::{
    archive::{ArchiveDecoder, ArchiveManifest, PageBytes},
    pdf::{PdfDecoder, PdfError},
};
use crate::store::sqlite::SqliteStore;

#[derive(Debug, thiserror::Error)]
pub enum ReaderError {
    #[error("reader capacity is busy; try again shortly")]
    Busy,
    #[error("library file or unit was not found")]
    NotFound,
    #[error("source cannot be read safely")]
    UnsafeSource,
    #[error("source changed; scan and associate it again")]
    StaleSource,
    #[error("document cannot be decoded within the reader limits")]
    InvalidDocument,
    #[error("PDF processing exceeded its time limit; try again later")]
    Timeout,
    #[error("encrypted PDFs are unsupported")]
    EncryptedDocument,
    #[error("page is outside the document")]
    PageBounds,
    #[error("progress changed; reload it before saving")]
    RevisionConflict,
    #[error("document signature changed; explicitly reset progress")]
    ResetRequired,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<PdfError> for ReaderError {
    fn from(error: PdfError) -> Self {
        match error {
            PdfError::Timeout => Self::Timeout,
            PdfError::Encrypted => Self::EncryptedDocument,
            PdfError::Invalid | PdfError::Cancelled => Self::InvalidDocument,
        }
    }
}

#[derive(Clone)]
pub struct ReaderService {
    store: SqliteStore,
    archive: ArchiveDecoder,
    archive_slots: Arc<Semaphore>,
    pdf: PdfDecoder,
    cache: Arc<Mutex<VecDeque<CachedManifest>>>,
    pages: Arc<Mutex<VecDeque<CachedPage>>>,
    thumbnails: Arc<Mutex<VecDeque<CachedThumbnail>>>,
    thumbnail_slots: Arc<Semaphore>,
    hash_slots: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    in_flight: Arc<std::sync::Mutex<HashMap<String, Weak<ReadGroup>>>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub file_id: String,
    pub signature: String,
    pub format: String,
    pub page_count: usize,
}

#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub signature: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SafeFileView {
    pub id: String,
    pub format: String,
    pub signature: String,
    pub size_bytes: i64,
}

/// Catalog placement of one unit a library file covers.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FileContext {
    pub publication_id: String,
    pub publication_title: String,
    pub content_type: String,
    pub edition_id: String,
    pub unit_id: String,
    pub unit_label: String,
    pub unit_kind: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Ltr,
    Rtl,
    Vertical,
}
impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ltr => "ltr",
            Self::Rtl => "rtl",
            Self::Vertical => "vertical",
        }
    }
    fn from_db(value: &str) -> Result<Self, ReaderError> {
        match value {
            "ltr" => Ok(Self::Ltr),
            "rtl" => Ok(Self::Rtl),
            "vertical" => Ok(Self::Vertical),
            _ => Err(ReaderError::InvalidDocument),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Progress {
    pub file_id: String,
    pub signature: String,
    pub page: usize,
    pub direction: Direction,
    pub revision: i64,
    pub reset_required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressUpdate {
    pub signature: String,
    pub page: usize,
    pub direction: Direction,
    pub revision: i64,
    #[serde(default)]
    pub reset: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRecord {
    id: String,
    root: PathBuf,
    relative: PathBuf,
    signature: String,
    format: String,
    size: i64,
    mtime: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl Stamp {
    fn read(metadata: &Metadata) -> Result<Self, ReaderError> {
        if !metadata.is_file() {
            return Err(ReaderError::UnsafeSource);
        }
        let mtime = metadata
            .mtime()
            .checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(metadata.mtime_nsec()))
            .ok_or(ReaderError::UnsafeSource)?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            mtime,
            ctime: metadata.ctime(),
            ctime_ns: metadata.ctime_nsec(),
        })
    }
}

struct Opened {
    file: Arc<File>,
    record: FileRecord,
    stamp: Stamp,
    _admission: Arc<ReadAdmission>,
}

struct ReadAdmission {
    _group: Arc<ReadGroup>,
    _waiter: OwnedSemaphorePermit,
    _file: OwnedMutexGuard<()>,
}

struct ReadGroup {
    _permit: OwnedSemaphorePermit,
    waiters: Arc<Semaphore>,
    file: Arc<Mutex<()>>,
}

impl Opened {
    fn open(record: FileRecord, admission: Arc<ReadAdmission>) -> Result<Self, ReaderError> {
        if record.relative.as_os_str().is_empty()
            || record
                .relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(ReaderError::UnsafeSource);
        }
        let root = std::fs::canonicalize(&record.root).map_err(|_| ReaderError::UnsafeSource)?;
        if root != record.root || !root.is_dir() {
            return Err(ReaderError::UnsafeSource);
        }
        let path = root.join(&record.relative);
        let before = std::fs::symlink_metadata(&path).map_err(|_| ReaderError::UnsafeSource)?;
        let before = Stamp::read(&before)?;
        // Linux O_NOFOLLOW | O_NONBLOCK prevents final symlink and FIFO races.
        let file = Arc::new(
            OpenOptions::new()
                .read(true)
                .custom_flags(0o400000 | 0o4000)
                .open(&path)
                .map_err(|_| ReaderError::UnsafeSource)?,
        );
        let stamp = Stamp::read(&file.metadata().map_err(|_| ReaderError::UnsafeSource)?)?;
        if before != stamp {
            return Err(ReaderError::StaleSource);
        }
        if stamp.size > 2 * 1024 * 1024 * 1024
            || i64::try_from(stamp.size).ok() != Some(record.size)
            || stamp.mtime != record.mtime
        {
            return Err(ReaderError::StaleSource);
        }
        let opened = Self {
            file,
            record,
            stamp,
            _admission: admission,
        };
        opened.verify()?;
        Ok(opened)
    }

    fn descriptor_path(&self) -> PathBuf {
        descriptor_path(&self.file)
    }

    fn verify(&self) -> Result<(), ReaderError> {
        let target =
            std::fs::canonicalize(self.descriptor_path()).map_err(|_| ReaderError::UnsafeSource)?;
        if !target.starts_with(&self.record.root) {
            return Err(ReaderError::UnsafeSource);
        }
        let current_root =
            std::fs::canonicalize(&self.record.root).map_err(|_| ReaderError::UnsafeSource)?;
        if current_root != self.record.root {
            return Err(ReaderError::UnsafeSource);
        }
        let actual = Stamp::read(
            &self
                .file
                .metadata()
                .map_err(|_| ReaderError::UnsafeSource)?,
        )?;
        let registered = std::fs::symlink_metadata(self.record.root.join(&self.record.relative))
            .map_err(|_| ReaderError::StaleSource)?;
        if actual != self.stamp || Stamp::read(&registered)? != self.stamp {
            return Err(ReaderError::StaleSource);
        }
        Ok(())
    }
}

fn descriptor_path(file: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}

#[derive(Clone)]
struct CachedManifest {
    record: FileRecord,
    stamp: Stamp,
    manifest: Manifest,
    archive: Option<Arc<ArchiveManifest>>,
}

struct CachedPage {
    record: FileRecord,
    stamp: Stamp,
    index: usize,
    page: PageBytes,
}

const PAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const THUMBNAIL_WIDTH: u32 = 240;
const THUMBNAIL_QUALITY: u8 = 80;
const THUMBNAIL_CACHE_ENTRIES: usize = 128;

struct CachedThumbnail {
    id: String,
    signature: String,
    bytes: Vec<u8>,
}

impl ReaderService {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            archive: ArchiveDecoder::new(),
            archive_slots: Arc::new(Semaphore::new(2)),
            pdf: PdfDecoder::new(),
            cache: Arc::new(Mutex::new(VecDeque::new())),
            pages: Arc::new(Mutex::new(VecDeque::new())),
            thumbnails: Arc::new(Mutex::new(VecDeque::new())),
            thumbnail_slots: Arc::new(Semaphore::new(2)),
            hash_slots: Arc::new(Semaphore::new(2)),
            admission: Arc::new(Semaphore::new(16)),
            in_flight: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn record(&self, id: &str) -> Result<FileRecord, ReaderError> {
        let row = sqlx::query("SELECT f.id, r.path AS root, f.relative_path, f.signature, f.format, f.size_bytes, f.mtime_ns FROM library_files f JOIN library_roots r ON r.id=f.root_id WHERE f.id=?")
            .bind(id).fetch_optional(self.store.reader()).await?.ok_or(ReaderError::NotFound)?;
        Ok(FileRecord {
            id: row.get("id"),
            root: PathBuf::from(row.get::<String, _>("root")),
            relative: PathBuf::from(
                row.get::<Option<String>, _>("relative_path")
                    .ok_or(ReaderError::UnsafeSource)?,
            ),
            signature: row.get("signature"),
            format: row.get("format"),
            size: row.get("size_bytes"),
            mtime: row
                .get::<Option<i64>, _>("mtime_ns")
                .ok_or(ReaderError::UnsafeSource)?,
        })
    }

    async fn current(&self, opened: &Opened) -> Result<(), ReaderError> {
        if self.record(&opened.record.id).await? != opened.record {
            return Err(ReaderError::StaleSource);
        }
        opened.verify()
    }

    async fn admit(&self, id: &str) -> Result<Arc<ReadAdmission>, ReaderError> {
        let group = {
            let mut active = self.in_flight.lock().map_err(|_| ReaderError::Busy)?;
            active.retain(|_, lock| lock.strong_count() > 0);
            match active.get(id).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let permit = self
                        .admission
                        .clone()
                        .try_acquire_owned()
                        .map_err(|_| ReaderError::Busy)?;
                    let lock = Arc::new(ReadGroup {
                        _permit: permit,
                        waiters: Arc::new(Semaphore::new(16)),
                        file: Arc::new(Mutex::new(())),
                    });
                    active.insert(id.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        let waiter = group
            .waiters
            .clone()
            .try_acquire_owned()
            .map_err(|_| ReaderError::Busy)?;
        let file = group.file.clone().lock_owned().await;
        Ok(Arc::new(ReadAdmission {
            _group: group,
            _waiter: waiter,
            _file: file,
        }))
    }

    async fn prepare(&self, id: &str) -> Result<(Opened, CachedManifest), ReaderError> {
        let admission = self.admit(id).await?;
        let record = self.record(id).await?;
        let opened = tokio::task::spawn_blocking(move || Opened::open(record, admission))
            .await
            .map_err(|_| ReaderError::UnsafeSource)??;
        let mut cache = self.cache.lock().await;
        if let Some(position) = cache
            .iter()
            .position(|entry| entry.record == opened.record && entry.stamp == opened.stamp)
        {
            let entry = cache.remove(position).expect("cache position exists");
            cache.push_back(entry.clone());
            return Ok((opened, entry));
        }
        cache.retain(|entry| entry.record.id != id);
        drop(cache);
        let file = opened.file.clone();
        let expected = opened.record.signature.clone();
        let size = opened.stamp.size;
        let admission = opened._admission.clone();
        let permit = self
            .hash_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ReaderError::UnsafeSource)?;
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            let result = verify_signature(&file, size, &expected);
            drop(permit);
            result
        })
        .await
        .map_err(|_| ReaderError::UnsafeSource)??;
        opened.verify()?;
        let (page_count, archive) = match opened.record.format.as_str() {
            "pdf" => (
                self.pdf
                    .manifest(opened.file.clone())
                    .await
                    .map_err(ReaderError::from)?,
                None,
            ),
            "cbz" | "cbr" => {
                let permit = self
                    .archive_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| ReaderError::InvalidDocument)?;
                let decoder = self.archive.clone();
                let file = opened.file.clone();
                let admission = opened._admission.clone();
                // ArchiveDecoder's API accepts paths. Keep the descriptor owned until its
                // bounded worker finishes, even when this request future is dropped.
                let manifest = tokio::spawn(async move {
                    let _admission = admission;
                    let result = decoder.manifest(&descriptor_path(&file)).await;
                    drop(file);
                    drop(permit);
                    result
                })
                .await
                .map_err(|_| ReaderError::InvalidDocument)?
                .map_err(|_| ReaderError::InvalidDocument)?;
                (manifest.pages.len(), Some(Arc::new(manifest)))
            }
            _ => return Err(ReaderError::InvalidDocument),
        };
        self.current(&opened).await?;
        let manifest = Manifest {
            file_id: opened.record.id.clone(),
            signature: opened.record.signature.clone(),
            format: opened.record.format.clone(),
            page_count,
        };
        let entry = CachedManifest {
            record: opened.record.clone(),
            stamp: opened.stamp,
            manifest,
            archive,
        };
        let mut cache = self.cache.lock().await;
        cache.retain(|cached| cached.record.id != id);
        if cache.len() == 32 {
            cache.pop_front();
        }
        cache.push_back(entry.clone());
        Ok((opened, entry))
    }

    pub async fn manifest(&self, id: &str) -> Result<Manifest, ReaderError> {
        let (opened, entry) = self.prepare(id).await?;
        self.current(&opened).await?;
        Ok(entry.manifest)
    }

    pub async fn metadata(
        &self,
        id: &str,
    ) -> Result<Option<crate::library::metadata::EmbeddedMetadata>, ReaderError> {
        let (opened, entry) = self.prepare(id).await?;
        let hints = if entry.archive.is_some() {
            crate::library::metadata::EmbeddedMetadata::read(&self.archive, opened.file.clone())
                .await
                .map_err(|_| ReaderError::InvalidDocument)?
        } else {
            None
        };
        self.current(&opened).await?;
        Ok(hints)
    }

    pub async fn page(&self, id: &str, page: usize) -> Result<PageBytes, ReaderError> {
        let (opened, entry) = self.prepare(id).await?;
        if page >= entry.manifest.page_count {
            return Err(ReaderError::PageBounds);
        }
        let cached = {
            let mut pages = self.pages.lock().await;
            let position = pages.iter().position(|cached| {
                cached.record == opened.record
                    && cached.stamp == opened.stamp
                    && cached.index == page
            });
            position.map(|position| {
                let cached = pages.remove(position).expect("cache entry exists");
                let result = cached.page.clone();
                pages.push_back(cached);
                result
            })
        };
        if let Some(cached) = cached {
            self.current(&opened).await?;
            return Ok(cached);
        }
        let result = if let Some(manifest) = entry.archive {
            // Bound detached descriptor owners as well as decoder processes.
            let permit = self
                .archive_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ReaderError::InvalidDocument)?;
            let name = manifest.pages[page].name.clone();
            let decoder = self.archive.clone();
            let file = opened.file.clone();
            let admission = opened._admission.clone();
            tokio::spawn(async move {
                let _admission = admission;
                let result = decoder.page(&descriptor_path(&file), &name).await;
                drop(file);
                drop(permit);
                result
            })
            .await
            .map_err(|_| ReaderError::InvalidDocument)?
            .map_err(|_| ReaderError::InvalidDocument)?
        } else {
            self.pdf
                .page(opened.file.clone(), page)
                .await
                .map_err(ReaderError::from)?
        };
        self.current(&opened).await?;
        let mut pages = self.pages.lock().await;
        pages.retain(|cached| !(cached.record.id == id && cached.index == page));
        let mut bytes: usize = pages.iter().map(|cached| cached.page.bytes.len()).sum();
        while pages.len() >= 16 || bytes + result.bytes.len() > PAGE_CACHE_BYTES {
            let Some(old) = pages.pop_front() else {
                break;
            };
            bytes -= old.page.bytes.len();
        }
        if result.bytes.len() <= PAGE_CACHE_BYTES {
            pages.push_back(CachedPage {
                record: opened.record,
                stamp: opened.stamp,
                index: page,
                page: result.clone(),
            });
        }
        Ok(result)
    }

    /// Registered signature of a file; used as the thumbnail validator.
    pub async fn file_signature(&self, id: &str) -> Result<String, ReaderError> {
        Ok(self.record(id).await?.signature)
    }

    /// First page downscaled to 240px wide JPEG. Rendering goes through `page`, so reader
    /// admission, signature verification, and archive/PDF limits apply unchanged.
    pub async fn thumbnail(&self, id: &str) -> Result<Thumbnail, ReaderError> {
        let signature = self.file_signature(id).await?;
        {
            let mut cache = self.thumbnails.lock().await;
            if let Some(position) = cache
                .iter()
                .position(|entry| entry.id == id && entry.signature == signature)
            {
                let entry = cache.remove(position).expect("cache position exists");
                let bytes = entry.bytes.clone();
                cache.push_back(entry);
                return Ok(Thumbnail { signature, bytes });
            }
        }
        let page = self.page(id, 0).await?;
        if self.file_signature(id).await? != signature {
            return Err(ReaderError::StaleSource);
        }
        let permit = self
            .thumbnail_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ReaderError::Busy)?;
        let bytes = tokio::task::spawn_blocking(move || {
            let result = encode_thumbnail(&page.bytes);
            drop(permit);
            result
        })
        .await
        .map_err(|_| ReaderError::InvalidDocument)??;
        let mut cache = self.thumbnails.lock().await;
        cache.retain(|entry| entry.id != id);
        while cache.len() >= THUMBNAIL_CACHE_ENTRIES {
            cache.pop_front();
        }
        cache.push_back(CachedThumbnail {
            id: id.to_owned(),
            signature: signature.clone(),
            bytes: bytes.clone(),
        });
        Ok(Thumbnail { signature, bytes })
    }

    pub async fn files(&self, unit_id: &str) -> Result<Vec<SafeFileView>, ReaderError> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM units WHERE id=?")
            .bind(unit_id)
            .fetch_optional(self.store.reader())
            .await?;
        if exists.is_none() {
            return Err(ReaderError::NotFound);
        }
        let rows = sqlx::query("SELECT f.id,f.format,f.signature,f.size_bytes FROM library_files f JOIN file_coverage c ON c.library_file_id=f.id WHERE c.unit_id=? ORDER BY f.id")
            .bind(unit_id).fetch_all(self.store.reader()).await?;
        Ok(rows
            .into_iter()
            .map(|row| SafeFileView {
                id: row.get("id"),
                format: row.get("format"),
                signature: row.get("signature"),
                size_bytes: row.get("size_bytes"),
            })
            .collect())
    }

    /// Empty when the file has no unit association yet.
    pub async fn context(&self, file_id: &str) -> Result<Vec<FileContext>, ReaderError> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM library_files WHERE id=?")
            .bind(file_id)
            .fetch_optional(self.store.reader())
            .await?;
        if exists.is_none() {
            return Err(ReaderError::NotFound);
        }
        let rows = sqlx::query("SELECT p.id AS publication_id, p.title AS publication_title, p.content_type, e.id AS edition_id, u.id AS unit_id, u.label AS unit_label, u.kind AS unit_kind FROM file_coverage c JOIN units u ON u.id=c.unit_id JOIN editions e ON e.id=u.edition_id JOIN publications p ON p.id=e.publication_id WHERE c.library_file_id=? ORDER BY p.sort_title, p.id, e.language, COALESCE(e.region, ''), e.id, COALESCE(u.sort_key, u.label), u.id")
            .bind(file_id).fetch_all(self.store.reader()).await?;
        Ok(rows
            .into_iter()
            .map(|row| FileContext {
                publication_id: row.get("publication_id"),
                publication_title: row.get("publication_title"),
                content_type: row.get("content_type"),
                edition_id: row.get("edition_id"),
                unit_id: row.get("unit_id"),
                unit_label: row.get("unit_label"),
                unit_kind: row.get("unit_kind"),
            })
            .collect())
    }

    pub async fn progress(&self, user_id: &str, id: &str) -> Result<Progress, ReaderError> {
        let (opened, entry) = self.prepare(id).await?;
        let row = sqlx::query("SELECT signature,page,direction,revision FROM reading_progress WHERE user_id=? AND file_id=?")
            .bind(user_id).bind(id).fetch_optional(self.store.reader()).await?;
        let mut progress = Progress {
            file_id: id.into(),
            signature: entry.manifest.signature,
            page: 0,
            direction: Direction::Ltr,
            revision: 0,
            reset_required: false,
        };
        if let Some(row) = row {
            progress.revision = row.get("revision");
            progress.reset_required = row.get::<String, _>("signature") != progress.signature;
            if !progress.reset_required {
                progress.page = row.get::<i64, _>("page") as usize;
                progress.direction = Direction::from_db(&row.get::<String, _>("direction"))?;
                if progress.page >= entry.manifest.page_count {
                    return Err(ReaderError::PageBounds);
                }
            }
        }
        self.current(&opened).await?;
        Ok(progress)
    }

    pub async fn save_progress(
        &self,
        user_id: &str,
        id: &str,
        input: ProgressUpdate,
    ) -> Result<Progress, ReaderError> {
        let (opened, entry) = self.prepare(id).await?;
        if input.signature != entry.manifest.signature {
            return Err(ReaderError::StaleSource);
        }
        if input.page >= entry.manifest.page_count {
            return Err(ReaderError::PageBounds);
        }
        if input.revision < 0 || input.revision == i64::MAX {
            return Err(ReaderError::RevisionConflict);
        }
        let mut transaction = self.store.begin_write().await?;
        let current = sqlx::query("SELECT f.signature,f.size_bytes,f.mtime_ns,f.relative_path,f.format,r.path FROM library_files f JOIN library_roots r ON r.id=f.root_id WHERE f.id=?")
            .bind(id).fetch_optional(&mut *transaction).await?.ok_or(ReaderError::NotFound)?;
        if current.get::<String, _>("signature") != opened.record.signature
            || current.get::<i64, _>("size_bytes") != opened.record.size
            || current.get::<Option<i64>, _>("mtime_ns") != Some(opened.record.mtime)
            || current
                .get::<Option<String>, _>("relative_path")
                .as_deref()
                .map(Path::new)
                != Some(opened.record.relative.as_path())
            || current.get::<String, _>("format") != opened.record.format
            || Path::new(&current.get::<String, _>("path")) != opened.record.root
        {
            return Err(ReaderError::StaleSource);
        }
        let prior = sqlx::query(
            "SELECT signature,revision FROM reading_progress WHERE user_id=? AND file_id=?",
        )
        .bind(user_id)
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await?;
        let revision = prior
            .as_ref()
            .map_or(0, |row| row.get::<i64, _>("revision"));
        if revision != input.revision {
            return Err(ReaderError::RevisionConflict);
        }
        if prior
            .as_ref()
            .is_some_and(|row| row.get::<String, _>("signature") != input.signature)
            && !input.reset
        {
            return Err(ReaderError::ResetRequired);
        }
        sqlx::query("INSERT INTO reading_progress (user_id,file_id,signature,page,direction,revision,updated_at) VALUES (?,?,?,?,?,?,unixepoch()) ON CONFLICT(user_id,file_id) DO UPDATE SET signature=excluded.signature,page=excluded.page,direction=excluded.direction,revision=excluded.revision,updated_at=excluded.updated_at")
            .bind(user_id).bind(id).bind(&input.signature).bind(input.page as i64).bind(input.direction.as_str()).bind(revision + 1)
            .execute(&mut *transaction).await?;
        opened.verify()?;
        transaction.commit().await?;
        Ok(Progress {
            file_id: id.into(),
            signature: input.signature,
            page: input.page,
            direction: input.direction,
            revision: revision + 1,
            reset_required: false,
        })
    }
}

fn encode_thumbnail(bytes: &[u8]) -> Result<Vec<u8>, ReaderError> {
    let image = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ReaderError::InvalidDocument)?
        .decode()
        .map_err(|_| ReaderError::InvalidDocument)?;
    let image = if image.width() > THUMBNAIL_WIDTH {
        let height =
            u64::from(image.height()) * u64::from(THUMBNAIL_WIDTH) / u64::from(image.width());
        let height = u32::try_from(height.max(1)).map_err(|_| ReaderError::InvalidDocument)?;
        image.resize_exact(
            THUMBNAIL_WIDTH,
            height,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let mut output = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, THUMBNAIL_QUALITY)
        .encode_image(&image.to_rgb8())
        .map_err(|_| ReaderError::InvalidDocument)?;
    Ok(output)
}

fn verify_signature(file: &File, size: u64, expected: &str) -> Result<(), ReaderError> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut offset = 0;
    while offset < size {
        let limit =
            usize::try_from((size - offset).min(buffer.len() as u64)).expect("bounded read size");
        let count = file
            .read_at(&mut buffer[..limit], offset)
            .map_err(|_| ReaderError::UnsafeSource)?;
        if count == 0 {
            return Err(ReaderError::StaleSource);
        }
        digest.update(&buffer[..count]);
        offset += count as u64;
    }
    let actual: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != expected {
        return Err(ReaderError::StaleSource);
    }
    Ok(())
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn pdf_errors_preserve_timeout_and_encryption() {
        assert!(matches!(
            ReaderError::from(PdfError::Timeout),
            ReaderError::Timeout
        ));
        assert!(matches!(
            ReaderError::from(PdfError::Encrypted),
            ReaderError::EncryptedDocument
        ));
        for error in [PdfError::Invalid, PdfError::Cancelled] {
            assert!(matches!(
                ReaderError::from(error),
                ReaderError::InvalidDocument
            ));
        }
    }

    #[test]
    fn pdf_errors_have_distinct_http_status_and_code() {
        use crate::httpapi::errors::ApiError;
        use axum::http::StatusCode;
        for (error, status, code, message) in [
            (
                PdfError::Timeout,
                StatusCode::SERVICE_UNAVAILABLE,
                "reader_timeout",
                "PDF processing exceeded its time limit; try again later",
            ),
            (
                PdfError::Encrypted,
                StatusCode::UNPROCESSABLE_ENTITY,
                "encrypted_document",
                "encrypted PDFs are unsupported",
            ),
            (
                PdfError::Invalid,
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_document",
                "document cannot be decoded within the reader limits",
            ),
            (
                PdfError::Cancelled,
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_document",
                "document cannot be decoded within the reader limits",
            ),
        ] {
            let api = ApiError::from(ReaderError::from(error));
            assert_eq!(api.status, status);
            assert_eq!(api.code, code);
            assert_eq!(api.message, message);
        }
        let busy = ApiError::from(ReaderError::Busy);
        assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(busy.code, "reader_busy");
    }

    #[test]
    fn thumbnails_downscale_to_240_wide_jpeg_without_upscaling() {
        for ((width, height), expected) in [
            ((1000, 500), (240, 120)),
            ((100, 50), (100, 50)),
            ((5000, 1), (240, 1)),
        ] {
            let mut png = Vec::new();
            image::DynamicImage::new_rgba8(width, height)
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            let jpeg = encode_thumbnail(&png).unwrap();
            assert_eq!(
                image::guess_format(&jpeg).unwrap(),
                image::ImageFormat::Jpeg
            );
            let thumbnail = image::load_from_memory(&jpeg).unwrap();
            assert_eq!((thumbnail.width(), thumbnail.height()), expected);
        }
        assert!(matches!(
            encode_thumbnail(b"not an image"),
            Err(ReaderError::InvalidDocument)
        ));
    }

    #[tokio::test]
    async fn one_file_has_bounded_waiters_without_exhausting_other_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&directory.path().join("state"))
            .await
            .unwrap();
        let service = ReaderService::new(store);
        let first = service.admit("slow-file").await.unwrap();
        let group = first._group.clone();
        let mut waiters = tokio::task::JoinSet::new();
        for _ in 0..15 {
            let service = service.clone();
            waiters.spawn(async move { service.admit("slow-file").await.map(drop) });
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while group.waiters.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(service.admission.available_permits(), 15);
        assert!(matches!(
            service.admit("slow-file").await,
            Err(ReaderError::Busy)
        ));
        let unrelated = service.admit("another-file").await.unwrap();
        drop(unrelated);
        drop(first);
        while let Some(result) = waiters.join_next().await {
            result.unwrap().unwrap();
        }
        drop(group);
        assert_eq!(service.admission.available_permits(), 16);
    }

    #[tokio::test]
    async fn capacity_is_checked_before_database_or_filesystem_access() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&directory.path().join("state"))
            .await
            .unwrap();
        let service = ReaderService::new(store.clone());
        let permits = service
            .admission
            .clone()
            .acquire_many_owned(16)
            .await
            .unwrap();
        store.close().await;
        assert!(matches!(
            service.manifest("missing").await,
            Err(ReaderError::Busy)
        ));
        drop(permits);
        assert!(matches!(
            service.manifest("missing").await,
            Err(ReaderError::Database(_))
        ));
        assert_eq!(service.admission.available_permits(), 16);
    }
}
