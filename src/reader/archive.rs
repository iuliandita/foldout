use std::{
    cmp::Ordering,
    collections::HashSet,
    fmt,
    fs::File,
    io::Cursor,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use image::{ImageFormat, ImageReader};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{Semaphore, oneshot},
    task::JoinHandle,
    time::sleep,
};

const MAX_PAGES: usize = 10_000;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_DIMENSION: u32 = 20_000;
const MAX_PIXELS: u64 = 64_000_000;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);

pub const MAX_COMIC_INFO_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct ArchiveDecoder {
    workers: Arc<Semaphore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveManifest {
    pub pages: Vec<ArchivePage>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePage {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageBytes {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum ArchiveError {
    UnsupportedPlatform,
    Process(String),
    TimedOut,
    OutputTooLarge,
    InvalidListing(String),
    NoPages,
    InvalidPageName,
    InvalidImage(String),
    WorkerCancelled,
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(formatter, "archive decoding is supported only on Linux")
            }
            Self::Process(message) => write!(formatter, "archive process failed: {message}"),
            Self::TimedOut => write!(formatter, "archive process timed out"),
            Self::OutputTooLarge => write!(formatter, "archive process output exceeded its limit"),
            Self::InvalidListing(message) => {
                write!(formatter, "invalid archive listing: {message}")
            }
            Self::NoPages => write!(formatter, "archive contains no supported image pages"),
            Self::InvalidPageName => write!(formatter, "invalid archive page name"),
            Self::InvalidImage(message) => write!(formatter, "invalid image: {message}"),
            Self::WorkerCancelled => write!(formatter, "archive worker was cancelled"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl Default for ArchiveDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveDecoder {
    pub fn new() -> Self {
        Self {
            workers: Arc::new(Semaphore::new(2)),
        }
    }

    /// Extracts one unambiguous ComicInfo.xml using the supervised archive worker.
    /// The descriptor stays owned through cancellation and child reaping.
    pub async fn comic_info(&self, file: Arc<File>) -> Result<Option<Vec<u8>>, ArchiveError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = file;
            Err(ArchiveError::UnsupportedPlatform)
        }
        #[cfg(target_os = "linux")]
        self.run(move |mut cancelled| async move {
            use std::os::{fd::AsRawFd, unix::fs::FileExt};

            if !file
                .metadata()
                .map_err(|error| ArchiveError::Process(error.to_string()))?
                .is_file()
            {
                return Err(ArchiveError::InvalidListing(
                    "source is not a regular file".into(),
                ));
            }
            let mut magic = [0_u8; 8];
            let read = file
                .read_at(&mut magic, 0)
                .map_err(|error| ArchiveError::Process(error.to_string()))?;
            let kind = if read >= 4
                && matches!(&magic[..4], b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
            {
                "-tzip"
            } else if read >= 7 && &magic[..7] == b"Rar!\x1a\x07\x00" {
                "-trar"
            } else if read == 8 && &magic == b"Rar!\x1a\x07\x01\x00" {
                "-tRar5"
            } else {
                return Err(ArchiveError::InvalidListing(
                    "source is not ZIP or RAR".into(),
                ));
            };
            let path = std::path::PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                file.as_raw_fd()
            ));
            let listing = run_7z(
                ["l", "-slt", "-ba", "-p-", kind, "--"],
                &path,
                None,
                MAX_LISTING_BYTES,
                &mut cancelled,
            )
            .await?;
            let Some(entry) = comic_info_entry(&listing)? else {
                return Ok(None);
            };
            let bytes = run_7z(
                ["x", "-so", "-spd", "-p-", kind, "--"],
                &path,
                Some(&entry.name),
                MAX_COMIC_INFO_BYTES,
                &mut cancelled,
            )
            .await?;
            // The declared length is only a hint; both extraction and equality are checked.
            if bytes.len() as u64 != entry.size {
                return Err(ArchiveError::InvalidListing("metadata size changed".into()));
            }
            drop(file);
            Ok(Some(bytes))
        })
        .await
    }

    pub async fn manifest(&self, path: &Path) -> Result<ArchiveManifest, ArchiveError> {
        let path = path.to_owned();
        self.run(move |mut cancelled| async move {
            let listing = run_7z(
                ["l", "-slt", "-ba", "-p-", "--"],
                &path,
                None,
                MAX_LISTING_BYTES,
                &mut cancelled,
            )
            .await?;
            parse_manifest(&listing)
        })
        .await
    }

    pub async fn page(&self, path: &Path, page_name: &str) -> Result<PageBytes, ArchiveError> {
        validate_page_path(page_name).map_err(|_| ArchiveError::InvalidPageName)?;
        if !is_image_path(page_name) {
            return Err(ArchiveError::InvalidPageName);
        }

        let path = path.to_owned();
        let page_name = page_name.to_owned();
        self.run(move |mut cancelled| async move {
            let listing = run_7z(
                ["l", "-slt", "-ba", "-p-", "--"],
                &path,
                None,
                MAX_LISTING_BYTES,
                &mut cancelled,
            )
            .await?;
            let manifest = parse_manifest(&listing)?;
            if !manifest.pages.iter().any(|page| page.name == page_name) {
                return Err(ArchiveError::InvalidPageName);
            }
            let bytes = run_7z(
                ["x", "-so", "-spd", "-p-", "--"],
                &path,
                Some(&page_name),
                MAX_IMAGE_BYTES,
                &mut cancelled,
            )
            .await?;
            page_bytes(bytes)
        })
        .await
    }

    async fn run<T, F, Fut>(&self, operation: F) -> Result<T, ArchiveError>
    where
        T: Send + 'static,
        F: FnOnce(oneshot::Receiver<()>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, ArchiveError>> + Send + 'static,
    {
        let permit = self
            .workers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ArchiveError::WorkerCancelled)?;
        let (mut result_tx, result_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let mut operation = tokio::spawn(operation(cancel_rx));
            let result = tokio::select! {
                result = &mut operation => join_operation(result),
                _ = result_tx.closed() => {
                    let _ = cancel_tx.send(());
                    join_operation(operation.await)
                }
            };
            drop(permit);
            let _ = result_tx.send(result);
        });

        result_rx
            .await
            .unwrap_or(Err(ArchiveError::WorkerCancelled))
    }
}

fn comic_info_entry(listing: &[u8]) -> Result<Option<ArchivePage>, ArchiveError> {
    let listing = std::str::from_utf8(listing)
        .map_err(|_| ArchiveError::InvalidListing("listing is not UTF-8".into()))?;
    let mut found = None;
    let mut names = HashSet::new();
    let mut total = 0_u64;
    for record in listing.split("\n\n") {
        let fields = record_fields(record)?;
        if fields.is_empty() {
            continue;
        }
        if field(&fields, "Encrypted").is_some_and(|value| value != "-")
            || ["Symbolic Link", "Hard Link", "Copy Link"]
                .iter()
                .any(|key| field(&fields, key).is_some_and(|value| !value.is_empty()))
            || field(&fields, "Alternate Stream").is_some_and(|value| value != "-")
            || field(&fields, "Attributes").is_some_and(|value| {
                value.starts_with('l') || value.split_whitespace().any(|part| part.starts_with('l'))
            })
        {
            return Err(ArchiveError::InvalidListing(
                "encrypted or linked entry".into(),
            ));
        }
        let name = field(&fields, "Path")
            .ok_or_else(|| ArchiveError::InvalidListing("entry has no path".into()))?;
        let directory = field(&fields, "Folder") == Some("+")
            || field(&fields, "Attributes").is_some_and(|value| value.starts_with('D'));
        let checked_name = if directory {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        validate_page_path(checked_name)?;
        if checked_name.contains(':') || !names.insert(checked_name.to_owned()) {
            return Err(ArchiveError::InvalidListing(
                "unsafe or duplicate entry".into(),
            ));
        }
        if names.len() > MAX_PAGES + 1 {
            return Err(ArchiveError::InvalidListing("too many entries".into()));
        }
        let size = field(&fields, "Size")
            .ok_or_else(|| ArchiveError::InvalidListing("entry has no size".into()))?
            .parse::<u64>()
            .map_err(|_| ArchiveError::InvalidListing("invalid entry size".into()))?;
        total = total
            .checked_add(size)
            .ok_or(ArchiveError::OutputTooLarge)?;
        if total > MAX_TOTAL_BYTES {
            return Err(ArchiveError::OutputTooLarge);
        }
        if checked_name
            .rsplit('/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("ComicInfo.xml"))
        {
            if directory || found.is_some() {
                return Err(ArchiveError::InvalidListing(
                    "ambiguous ComicInfo.xml".into(),
                ));
            }
            if size > MAX_COMIC_INFO_BYTES as u64 {
                return Err(ArchiveError::OutputTooLarge);
            }
            found = Some(ArchivePage {
                name: name.to_owned(),
                size,
            });
        }
    }
    Ok(found)
}

fn join_operation<T>(
    result: Result<Result<T, ArchiveError>, tokio::task::JoinError>,
) -> Result<T, ArchiveError> {
    result.map_err(|error| ArchiveError::Process(error.to_string()))?
}

async fn run_7z<'a>(
    args: impl IntoIterator<Item = &'a str>,
    archive: &Path,
    entry: Option<&str>,
    stdout_limit: usize,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<Vec<u8>, ArchiveError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (args, archive, entry, stdout_limit, cancelled);
        return Err(ArchiveError::UnsupportedPlatform);
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("prlimit");
        command
            .args(["--as=536870912", "--cpu=10", "--", "7z"])
            .args(args)
            .arg(archive);
        if let Some(entry) = entry {
            command.arg(entry);
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| ArchiveError::Process(error.to_string()))?;
        supervise_child(child, stdout_limit, cancelled).await
    }
}

#[cfg(target_os = "linux")]
async fn supervise_child(
    mut child: Child,
    stdout_limit: usize,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<Vec<u8>, ArchiveError> {
    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    let stdout_reader = tokio::spawn(read_limited(stdout, stdout_limit));
    let stderr_reader = tokio::spawn(read_limited(stderr, MAX_STDERR_BYTES));

    let mut stdout_reader = Some(stdout_reader);
    let mut stderr_reader = Some(stderr_reader);
    let mut stdout_bytes: Option<Vec<u8>> = None;
    let mut stderr_bytes: Option<Vec<u8>> = None;
    let mut exit_status: Option<std::process::ExitStatus> = None;
    let deadline = sleep(PROCESS_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        if let Some(status) = exit_status.as_ref()
            && stdout_reader.is_none()
            && stderr_reader.is_none()
        {
            let stdout = stdout_bytes.take().expect("stdout reader completed");
            let stderr = stderr_bytes.take().expect("stderr reader completed");
            if !status.success() {
                return Err(ArchiveError::Process(lossy_stderr(&stderr, status.code())));
            }
            return Ok(stdout);
        }
        tokio::select! {
            status = child.wait(), if exit_status.is_none() => {
                match status {
                    Ok(status) => exit_status = Some(status),
                    Err(error) => {
                        reap_after_stop(&mut child, &mut stdout_reader, &mut stderr_reader).await;
                        return Err(ArchiveError::Process(error.to_string()));
                    }
                }
            }
            result = async { stdout_reader.as_mut().expect("reader exists").await }, if stdout_reader.is_some() => {
                // A completed JoinHandle must never be polled again during cleanup.
                stdout_reader = None;
                match reader_result(result) {
                    Ok(bytes) => {
                        stdout_bytes = Some(bytes);
                    }
                    Err(error) => {
                        reap_after_stop(&mut child, &mut stdout_reader, &mut stderr_reader).await;
                        return Err(error);
                    }
                }
            }
            result = async { stderr_reader.as_mut().expect("reader exists").await }, if stderr_reader.is_some() => {
                stderr_reader = None;
                match reader_result(result) {
                    Ok(bytes) => {
                        stderr_bytes = Some(bytes);
                    }
                    Err(error) => {
                        reap_after_stop(&mut child, &mut stdout_reader, &mut stderr_reader).await;
                        return Err(error);
                    }
                }
            }
            _ = &mut deadline => {
                reap_after_stop(&mut child, &mut stdout_reader, &mut stderr_reader).await;
                return Err(ArchiveError::TimedOut);
            }
            _ = &mut *cancelled => {
                reap_after_stop(&mut child, &mut stdout_reader, &mut stderr_reader).await;
                return Err(ArchiveError::WorkerCancelled);
            }
        }
    }
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn reap_after_stop(
    child: &mut Child,
    stdout_reader: &mut Option<JoinHandle<Result<Vec<u8>, ArchiveError>>>,
    stderr_reader: &mut Option<JoinHandle<Result<Vec<u8>, ArchiveError>>>,
) {
    stop_child(child).await;
    if let Some(reader) = stdout_reader.take() {
        reader.abort();
        let _ = reader.await;
    }
    if let Some(reader) = stderr_reader.take() {
        reader.abort();
        let _ = reader.await;
    }
}

fn reader_result(
    result: Result<Result<Vec<u8>, ArchiveError>, tokio::task::JoinError>,
) -> Result<Vec<u8>, ArchiveError> {
    result.map_err(|error| ArchiveError::Process(error.to_string()))?
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<Vec<u8>, ArchiveError> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|error| ArchiveError::Process(error.to_string()))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(ArchiveError::OutputTooLarge);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

fn lossy_stderr(stderr: &[u8], status: Option<i32>) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        format!("7z exited with status {}", status.unwrap_or(-1))
    } else {
        message
    }
}

pub(crate) fn parse_manifest(listing: &[u8]) -> Result<ArchiveManifest, ArchiveError> {
    let listing = std::str::from_utf8(listing)
        .map_err(|_| ArchiveError::InvalidListing("listing is not UTF-8".into()))?;
    let mut pages = Vec::new();
    let mut names = HashSet::new();
    let mut total_bytes = 0_u64;

    for record in listing.split("\n\n") {
        let fields = record_fields(record)?;
        if fields.is_empty() {
            continue;
        }
        if field(&fields, "Encrypted").is_some_and(|value| value != "-") {
            return Err(ArchiveError::InvalidListing("encrypted entry".into()));
        }
        if field(&fields, "Symbolic Link").is_some() || field(&fields, "Hard Link").is_some() {
            return Err(ArchiveError::InvalidListing("linked entry".into()));
        }

        let Some(name) = field(&fields, "Path") else {
            continue;
        };
        if !is_image_path(name) {
            continue;
        }
        validate_page_path(name)?;
        if field(&fields, "Attributes").is_some_and(|value| value.starts_with('l')) {
            return Err(ArchiveError::InvalidListing("symbolic link".into()));
        }
        if !names.insert(name.to_owned()) {
            return Err(ArchiveError::InvalidListing("duplicate page name".into()));
        }
        let size = field(&fields, "Size")
            .ok_or_else(|| ArchiveError::InvalidListing("page has no size".into()))?
            .parse::<u64>()
            .map_err(|_| ArchiveError::InvalidListing("page size is invalid".into()))?;
        if size > MAX_IMAGE_BYTES as u64 {
            return Err(ArchiveError::OutputTooLarge);
        }
        if pages.len() == MAX_PAGES {
            return Err(ArchiveError::InvalidListing("too many pages".into()));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or(ArchiveError::OutputTooLarge)?;
        if total_bytes > MAX_TOTAL_BYTES {
            return Err(ArchiveError::OutputTooLarge);
        }
        pages.push(ArchivePage {
            name: name.to_owned(),
            size,
        });
    }

    if pages.is_empty() {
        return Err(ArchiveError::NoPages);
    }
    pages.sort_by(|left, right| natural_cmp(&left.name, &right.name));
    Ok(ArchiveManifest { pages, total_bytes })
}

fn record_fields(record: &str) -> Result<Vec<(&str, &str)>, ArchiveError> {
    let mut fields = Vec::new();
    let mut names = HashSet::new();
    for line in record.lines() {
        let (name, value) = line
            .split_once(" = ")
            .ok_or_else(|| ArchiveError::InvalidListing("malformed record".into()))?;
        if name.is_empty()
            || value.bytes().any(|byte| byte.is_ascii_control())
            || !names.insert(name)
        {
            return Err(ArchiveError::InvalidListing("ambiguous record".into()));
        }
        fields.push((name, value));
    }
    Ok(fields)
}

fn field<'a>(fields: &'a [(&str, &str)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|(key, value)| (*key == name).then_some(*value))
}

fn validate_page_path(name: &str) -> Result<(), ArchiveError> {
    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ArchiveError::InvalidListing("unsafe page path".into()));
    }
    let path = Path::new(name);
    if name.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(ArchiveError::InvalidListing("unsafe page path".into()));
    }
    Ok(())
}

fn is_image_path(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp"
            )
        })
}

fn page_bytes(bytes: Vec<u8>) -> Result<PageBytes, ArchiveError> {
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| ArchiveError::InvalidImage(error.to_string()))?;
    let format = reader
        .format()
        .ok_or_else(|| ArchiveError::InvalidImage("unrecognized image format".into()))?;
    let content_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => {
            return Err(ArchiveError::InvalidImage(
                "unsupported image format".into(),
            ));
        }
    };
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| ArchiveError::InvalidImage(error.to_string()))?;
    if width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS
    {
        return Err(ArchiveError::InvalidImage(
            "image dimensions exceed limit".into(),
        ));
    }
    Ok(PageBytes {
        bytes,
        content_type,
        width,
        height,
    })
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_ascii_lowercase();
    let right = right.to_ascii_lowercase();
    let mut left_at = 0;
    let mut right_at = 0;
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    while left_at < left_bytes.len() && right_at < right_bytes.len() {
        let left_digit = left_bytes[left_at].is_ascii_digit();
        let right_digit = right_bytes[right_at].is_ascii_digit();
        if left_digit && right_digit {
            let left_end = run_digits(left_bytes, left_at);
            let right_end = run_digits(right_bytes, right_at);
            let left_number = trim_zeroes(&left_bytes[left_at..left_end]);
            let right_number = trim_zeroes(&right_bytes[right_at..right_end]);
            let order = left_number
                .len()
                .cmp(&right_number.len())
                .then_with(|| left_number.cmp(right_number));
            if order != Ordering::Equal {
                return order;
            }
            left_at = left_end;
            right_at = right_end;
        } else {
            let order = left_bytes[left_at].cmp(&right_bytes[right_at]);
            if order != Ordering::Equal {
                return order;
            }
            left_at += 1;
            right_at += 1;
        }
    }
    left_bytes
        .len()
        .cmp(&right_bytes.len())
        .then_with(|| left.cmp(&right))
}

fn run_digits(bytes: &[u8], start: usize) -> usize {
    start
        + bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
}

fn trim_zeroes(number: &[u8]) -> &[u8] {
    let trimmed = number
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(number.len() - 1);
    &number[trimmed..]
}

#[cfg(all(test, target_os = "linux"))]
mod supervisor_tests {
    use super::*;

    fn child(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    fn assert_reaped(pid: u32) {
        let mut status = 0;
        // A reaped child must no longer be waitable by this process.
        let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(result, -1, "child was still running or waitable");
        assert_eq!(error, Some(libc::ECHILD));
    }

    async fn oversized_pipe(script: &str, exited: bool) {
        let mut child = child(script);
        let pid = child.id().unwrap();
        // Child::wait closes its stdin; retain it separately to keep `read` blocked.
        let stdin = child.stdin.take().unwrap();
        if exited {
            // Output fits in the pipe: make child.wait() ready before supervision starts.
            child.wait().await.unwrap();
        }
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            supervise_child(child, 64, &mut cancel_rx),
        )
        .await
        .expect("pipe overflow cleanup hung");
        assert!(
            matches!(result, Err(ArchiveError::OutputTooLarge)),
            "{result:?}"
        );
        assert_reaped(pid);
        drop(stdin);
    }

    #[tokio::test]
    async fn oversized_stdout_reaps_child_without_repolling_completed_reader() {
        // Keep the child and its other pipe open until the supervisor stops them.
        oversized_pipe("printf '%065d' 0; read -r line", false).await;
    }

    #[tokio::test]
    async fn oversized_stderr_reaps_child_without_repolling_completed_reader() {
        oversized_pipe("printf '%016385d' 0 >&2; read -r line", false).await;
    }

    #[tokio::test]
    async fn exited_child_stdout_overflow_still_joins_both_readers() {
        oversized_pipe("printf '%065d' 0", true).await;
    }

    #[tokio::test]
    async fn exited_child_drains_both_pipes_before_reporting_failure() {
        let mut child = child("printf output; printf diagnostic >&2; exit 7");
        child.wait().await.unwrap();
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            supervise_child(child, 64, &mut cancel_rx),
        )
        .await
        .expect("exited child pipe draining hung");
        assert!(matches!(result, Err(ArchiveError::Process(message)) if message == "diagnostic"));
    }
}
