use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Result as IoResult},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::task::JoinSet;
use uuid::Uuid;

use super::{
    LibraryFormat,
    roots::{Library, LibraryError, canonical_directory},
};

const BATCH_SIZE: usize = 32;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanReport {
    pub run_id: String,
    pub visited: u64,
    pub skipped: u64,
    pub errors: u64,
}

#[derive(Clone, Debug)]
struct Candidate {
    relative: String,
    path: PathBuf,
    format: Option<LibraryFormat>,
    state: &'static str,
    reason: Option<&'static str>,
}
#[derive(Clone, Debug)]
struct Entry {
    relative: String,
    format: Option<LibraryFormat>,
    signature: Option<String>,
    size: Option<i64>,
    mtime: Option<i64>,
    state: &'static str,
    reason: Option<&'static str>,
}
#[derive(Clone, Debug)]
struct Prior {
    size: i64,
    mtime: i64,
    signature: String,
}
#[derive(Clone, Debug)]
pub(crate) struct HashResult {
    pub signature: String,
    pub size: i64,
    pub mtime_ns: i64,
}
struct ScanCancellation(Arc<AtomicBool>);

impl Drop for ScanCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct Discovery {
    directories: VecDeque<PathBuf>,
    current: Option<(PathBuf, fs::ReadDir)>,
}

impl Discovery {
    fn new(root: &Path) -> Self {
        Self {
            directories: VecDeque::from([root.to_path_buf()]),
            current: None,
        }
    }
    fn has_work(&self) -> bool {
        self.current.is_some() || !self.directories.is_empty()
    }
}

impl Library {
    pub async fn scan(&self, root_id: &str) -> Result<ScanReport, LibraryError> {
        let cancellation = ScanCancellation(Arc::new(AtomicBool::new(false)));
        let root = self.root(root_id).await?;
        let verified = canonical_directory(&root.path)?;
        if verified != root.path {
            return Err(LibraryError::InvalidRoot);
        }
        let run_id = Uuid::new_v4().to_string();
        self.begin_run(&run_id, root_id).await?;
        match self
            .scan_running(root_id, &run_id, &verified, cancellation.0.clone())
            .await
        {
            Ok(report) => {
                self.finish_run(root_id, &report).await?;
                Ok(report)
            }
            Err(error) => {
                self.fail_run(&run_id, &error).await?;
                Err(error)
            }
        }
    }

    async fn scan_running(
        &self,
        root_id: &str,
        run_id: &str,
        root: &Path,
        cancellation: Arc<AtomicBool>,
    ) -> Result<ScanReport, LibraryError> {
        let prior = self.prior(root_id).await?;
        let mut discovery = Discovery::new(root);
        let mut report = ScanReport {
            run_id: run_id.into(),
            ..Default::default()
        };
        while discovery.has_work() {
            let ((candidates, _discovery_errors), discovered) = tokio::task::spawn_blocking({
                let root = root.to_path_buf();
                move || {
                    let (candidates, errors, discovery) = discover_batch(&root, discovery);
                    ((candidates, errors), discovery)
                }
            })
            .await
            .map_err(|_| std::io::Error::other("scan worker failed"))?;
            discovery = discovered;
            let mut tasks = JoinSet::new();
            for candidate in candidates {
                let previous = prior.get(&candidate.relative).cloned();
                let root = root.to_path_buf();
                let slots = self.hash_slots.clone();
                let cancellation = cancellation.clone();
                tasks.spawn(async move {
                    let permit = slots
                        .acquire_owned()
                        .await
                        .expect("hash semaphore remains open");
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        inventory_candidate(&root, candidate, previous, &cancellation)
                    })
                    .await
                    .map_err(|_| std::io::Error::other("inventory worker failed"))
                });
            }
            let mut entries = Vec::new();
            while let Some(result) = tasks.join_next().await {
                let task = result.map_err(|_| std::io::Error::other("inventory task failed"))?;
                let entry = task??;
                report.visited += 1;
                if entry.state != "pending_association" {
                    report.skipped += 1;
                }
                if entry.state == "error" || entry.reason == Some("directory_unreadable") {
                    report.errors += 1;
                }
                entries.push(entry);
            }
            self.persist_entries(root_id, run_id, entries).await?;
        }
        Ok(report)
    }

    async fn begin_run(&self, id: &str, root_id: &str) -> Result<(), LibraryError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query(
            "INSERT INTO scan_runs (id, root_id, state, started_at) VALUES (?, ?, 'running', ?)",
        )
        .bind(id)
        .bind(root_id)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn prior(&self, root_id: &str) -> Result<HashMap<String, Prior>, LibraryError> {
        Ok(sqlx::query("SELECT relative_path, size_bytes, mtime_ns, signature FROM scan_entries WHERE root_id = ? AND state = 'pending_association'").bind(root_id).fetch_all(self.store.reader()).await?.into_iter().filter_map(|row| Some((row.get::<String, _>("relative_path"), Prior { size: row.get::<Option<i64>, _>("size_bytes")?, mtime: row.get::<Option<i64>, _>("mtime_ns")?, signature: row.get::<Option<String>, _>("signature")? }))).collect())
    }
    async fn persist_entries(
        &self,
        root_id: &str,
        run_id: &str,
        entries: Vec<Entry>,
    ) -> Result<(), LibraryError> {
        let mut tx = self.store.begin_write().await?;
        for entry in entries {
            sqlx::query("INSERT INTO scan_entries (id, root_id, relative_path, format, signature, size_bytes, mtime_ns, state, reason, last_seen_run_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(root_id, relative_path) DO UPDATE SET format=excluded.format, signature=excluded.signature, size_bytes=excluded.size_bytes, mtime_ns=excluded.mtime_ns, state=excluded.state, reason=excluded.reason, last_seen_run_id=excluded.last_seen_run_id").bind(Uuid::new_v4().to_string()).bind(root_id).bind(&entry.relative).bind(entry.format.as_ref().map(LibraryFormat::as_str)).bind(entry.signature).bind(entry.size).bind(entry.mtime).bind(entry.state).bind(entry.reason).bind(run_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    async fn finish_run(&self, root_id: &str, report: &ScanReport) -> Result<(), LibraryError> {
        let mut tx = self.store.begin_write().await?;
        if report.errors == 0 {
            sqlx::query("UPDATE scan_entries SET state = 'missing', reason = 'not_seen_in_successful_scan' WHERE root_id = ? AND last_seen_run_id IS NOT ? AND state != 'skipped'").bind(root_id).bind(&report.run_id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE scan_runs SET state = 'completed', visited = ?, skipped = ?, errors = ?, finished_at = ? WHERE id = ?").bind(report.visited as i64).bind(report.skipped as i64).bind(report.errors as i64).bind(now()).bind(&report.run_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn fail_run(&self, id: &str, error: &LibraryError) -> Result<(), LibraryError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query("UPDATE scan_runs SET state = 'failed', errors = errors + 1, finished_at = ? WHERE id = ?").bind(now()).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        tracing::warn!(error = %error, "library scan failed");
        Ok(())
    }
}

fn discover_batch(root: &Path, mut discovery: Discovery) -> (Vec<Candidate>, u64, Discovery) {
    let mut results = Vec::new();
    let mut errors = 0;
    while results.len() < BATCH_SIZE {
        if discovery.current.is_none() {
            let Some(directory) = discovery.directories.pop_front() else {
                break;
            };
            match safe_directory(root, &directory).and_then(|_| fs::read_dir(&directory)) {
                Ok(entries) => discovery.current = Some((directory, entries)),
                Err(error) => {
                    results.push(candidate(
                        root,
                        directory,
                        None,
                        "error",
                        Some(if error.kind() == std::io::ErrorKind::PermissionDenied {
                            "directory_unreadable"
                        } else {
                            classify(&error)
                        }),
                    ));
                    errors += 1;
                    continue;
                }
            }
        }
        let (directory, next) = {
            let (directory, entries) = discovery
                .current
                .as_mut()
                .expect("current directory exists");
            (directory.clone(), entries.next())
        };
        match next {
            None => discovery.current = None,
            Some(Ok(entry)) => {
                let path = entry.path();
                let ty = match fs::symlink_metadata(&path) {
                    Ok(value) => value.file_type(),
                    Err(_) => {
                        results.push(candidate(
                            root,
                            path,
                            None,
                            "error",
                            Some("metadata_unreadable"),
                        ));
                        errors += 1;
                        continue;
                    }
                };
                if ty.is_symlink() {
                    results.push(candidate(
                        root,
                        path,
                        None,
                        "skipped",
                        Some("symlink_skipped"),
                    ));
                } else if ty.is_dir() {
                    discovery.directories.push_back(path);
                } else if let Some(format) = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(str::to_ascii_lowercase)
                    .and_then(|value| value.parse().ok())
                {
                    results.push(candidate(
                        root,
                        path,
                        Some(format),
                        "pending_association",
                        None,
                    ));
                }
            }
            Some(Err(_)) => {
                results.push(Candidate {
                    relative: format!("<read_dir_error:{}>", errors),
                    path: directory,
                    format: None,
                    state: "error",
                    reason: Some("directory_entry_unreadable"),
                });
                errors += 1;
            }
        }
    }
    (results, errors, discovery)
}
fn candidate(
    root: &Path,
    path: PathBuf,
    format: Option<LibraryFormat>,
    state: &'static str,
    reason: Option<&'static str>,
) -> Candidate {
    let Some(relative_path) = path.strip_prefix(root).ok() else {
        return Candidate {
            relative: "<escaping_path>".into(),
            path,
            format: None,
            state: "skipped",
            reason: Some("escaping_path"),
        };
    };
    let Some(relative) = relative_path.to_str().map(str::to_owned) else {
        return Candidate {
            relative: format!(
                "<non_utf8_path:{}>",
                hex(Sha256::digest(relative_path.as_os_str().as_encoded_bytes()).as_slice())
            ),
            path,
            format: None,
            state: "skipped",
            reason: Some("non_utf8_path"),
        };
    };
    Candidate {
        relative,
        path,
        format,
        state,
        reason,
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn inventory_candidate(
    root: &Path,
    candidate: Candidate,
    prior: Option<Prior>,
    cancellation: &AtomicBool,
) -> IoResult<Entry> {
    if candidate.state != "pending_association" {
        return Ok(Entry {
            relative: candidate.relative,
            format: candidate.format,
            signature: None,
            size: None,
            mtime: None,
            state: candidate.state,
            reason: candidate.reason,
        });
    }
    if cancellation.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("scan cancelled"));
    }
    let current = match safe_metadata(root, &candidate.path) {
        Ok(value) => value,
        Err(error) => {
            return Ok(Entry {
                relative: candidate.relative,
                format: candidate.format,
                signature: None,
                size: None,
                mtime: None,
                state: "error",
                reason: Some(classify(&error)),
            });
        }
    };
    if prior
        .as_ref()
        .is_some_and(|previous| previous.size == current.size && previous.mtime == current.mtime_ns)
    {
        return Ok(Entry {
            relative: candidate.relative,
            format: candidate.format,
            signature: prior.map(|value| value.signature),
            size: Some(current.size),
            mtime: Some(current.mtime_ns),
            state: "pending_association",
            reason: Some("metadata_unchanged"),
        });
    }
    match hash_safe_cancelled(root, &candidate.path, cancellation) {
        Ok(result) => Ok(Entry {
            relative: candidate.relative,
            format: candidate.format,
            signature: Some(result.signature),
            size: Some(result.size),
            mtime: Some(result.mtime_ns),
            state: "pending_association",
            reason: Some("unmatched"),
        }),
        Err(error) => Ok(Entry {
            relative: candidate.relative,
            format: candidate.format,
            signature: None,
            size: None,
            mtime: None,
            state: "error",
            reason: Some(classify(&error)),
        }),
    }
}
fn classify(error: &std::io::Error) -> &'static str {
    match error.to_string().as_str() {
        "source escaped root" => "source_escaped_root",
        "source changed" => "source_changed",
        "symlink" => "symlink_replaced",
        "not a regular file" => "not_regular_file",
        _ => "source_unreadable",
    }
}
fn safe_directory(root: &Path, path: &Path) -> IoResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("symlink"));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let verified = fs::canonicalize(path)?;
    if verified != path || !verified.starts_with(root) {
        return Err(std::io::Error::other("source escaped root"));
    }
    Ok(())
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Stamp {
    size: i64,
    mtime_ns: i64,
    dev: u64,
    ino: u64,
}
#[cfg(target_os = "linux")]
fn stamp(metadata: &fs::Metadata) -> IoResult<Stamp> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(Stamp {
        size: metadata
            .len()
            .try_into()
            .map_err(|_| std::io::Error::other("source unreadable"))?,
        mtime_ns: metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos()
            .try_into()
            .map_err(|_| std::io::Error::other("source unreadable"))?,
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}
#[cfg(target_os = "linux")]
fn safe_open(root: &Path, path: &Path) -> IoResult<(File, Stamp)> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let before = fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() {
        return Err(std::io::Error::other("symlink"));
    }
    let path_stamp = stamp(&before)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0o404000)
        .open(path)?;
    let fd_stamp = stamp(&file.metadata()?)?;
    if path_stamp.dev != fd_stamp.dev || path_stamp.ino != fd_stamp.ino {
        return Err(std::io::Error::other("source changed"));
    }
    let target = fs::canonicalize(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if !target.starts_with(root)
        || !target
            .parent()
            .is_some_and(|parent| parent.starts_with(root))
    {
        return Err(std::io::Error::other("source escaped root"));
    }
    Ok((file, fd_stamp))
}
#[cfg(target_os = "linux")]
fn verify_path(path: &Path, expected: Stamp) -> IoResult<()> {
    let actual = stamp(&fs::symlink_metadata(path)?)?;
    if actual.dev != expected.dev
        || actual.ino != expected.ino
        || actual.size != expected.size
        || actual.mtime_ns != expected.mtime_ns
    {
        Err(std::io::Error::other("source changed"))
    } else {
        Ok(())
    }
}
#[cfg(target_os = "linux")]
fn safe_metadata(root: &Path, path: &Path) -> IoResult<HashResult> {
    let (_file, stamp) = safe_open(root, path)?;
    verify_path(path, stamp)?;
    Ok(HashResult {
        signature: String::new(),
        size: stamp.size,
        mtime_ns: stamp.mtime_ns,
    })
}
#[cfg(not(target_os = "linux"))]
fn safe_metadata(_root: &Path, _path: &Path) -> IoResult<HashResult> {
    Err(std::io::Error::other(
        "safe inventory hashing is only supported on Linux",
    ))
}
pub(crate) fn hash_safe(root: &Path, path: &Path) -> IoResult<HashResult> {
    hash_safe_cancelled(root, path, &AtomicBool::new(false))
}
fn hash_safe_cancelled(
    root: &Path,
    path: &Path,
    cancellation: &AtomicBool,
) -> IoResult<HashResult> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, path, cancellation);
        return Err(std::io::Error::other(
            "safe inventory hashing is only supported on Linux",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let (mut file, before) = safe_open(root, path)?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancellation.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("scan cancelled"));
            }
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let after = stamp(&file.metadata()?)?;
        if after.dev != before.dev
            || after.ino != before.ino
            || after.size != before.size
            || after.mtime_ns != before.mtime_ns
        {
            return Err(std::io::Error::other("source changed"));
        }
        verify_path(path, before)?;
        Ok(HashResult {
            signature: hex(digest.finalize().as_slice()),
            size: before.size,
            mtime_ns: before.mtime_ns,
        })
    }
}
