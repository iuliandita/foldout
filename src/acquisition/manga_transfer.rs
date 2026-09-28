//! Ephemeral MangaDex transfer and packaging. The caller owns durable state/publication.
use super::direct::{client_builder, public_ip};
use crate::providers::mangadex_chapters::{MangaDexManifest, MangaDexManifestSummary};
use image::{ImageFormat, ImageReader};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
    time::Instant,
};

const PAGE_BYTES: u64 = 32 * 1024 * 1024;
const TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const ARCHIVE_BYTES: u64 = TOTAL_BYTES + 8 * 1024 * 1024;
const MAX_PAGES: u32 = 1000;
const TRANSFER_TIME: Duration = Duration::from_secs(15 * 60);
const PACKAGE_TIME: Duration = Duration::from_secs(60);
const PIPE_BYTES: u64 = 16 * 1024;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum MangaTransferError {
    #[error("chapter manifest identity or page count changed")]
    Changed,
    #[error("chapter server violates network policy")]
    NetworkPolicy,
    #[error("chapter page transfer failed")]
    Transfer,
    #[error("chapter transfer exceeds size limits")]
    SizeLimit,
    #[error("chapter page is not a supported complete image")]
    InvalidImage,
    #[error("chapter packaging failed")]
    Package,
    #[error("chapter transfer exceeded its deadline")]
    TimedOut,
    #[error("chapter transfer was cancelled")]
    Cancelled,
    #[error("chapter staging file requires local review")]
    LocalReview,
}

/// Transfer an opaque manifest into an already exclusively opened, empty, private
/// regular output file. Only success permits the caller to publish/import that file.
/// Scratch pages and packaging stay beneath the supplied service-owned 0700 job
/// directory descriptor, on the download filesystem rather than system temporary storage.
/// On failure/cancellation the supervisor truncates the output after reaping 7z.
/// Dropping this future signals cancellation; it never abandons an active 7z child.
pub async fn transfer(
    manifest: &MangaDexManifest,
    output: &File,
    staging_parent: &File,
) -> Result<(), MangaTransferError> {
    start(manifest, output, staging_parent, Options::default()).await
}

/// Only image GETs are redirected to the owned fixture. Never compiled in production.
#[cfg(test)]
pub(crate) async fn transfer_fixture(
    manifest: &MangaDexManifest,
    output: &File,
    staging_parent: &File,
    fixture: SocketAddr,
) -> Result<(), MangaTransferError> {
    start(
        manifest,
        output,
        staging_parent,
        Options {
            fixture: Some(fixture),
            ..Options::default()
        },
    )
    .await
}

// No Debug/Serialize: the private snapshot lets cancellation cleanup own all data
// without adding Clone or a URL export to the provider's opaque handle.
struct Snapshot {
    summary: MangaDexManifestSummary,
    identity: String,
    pages: Vec<reqwest::Url>,
}
impl Snapshot {
    fn new(manifest: &MangaDexManifest) -> Result<Self, MangaTransferError> {
        let count = manifest.summary().page_count;
        if !(1..=MAX_PAGES).contains(&count) {
            return Err(MangaTransferError::Changed);
        }
        let snapshot = Self {
            summary: manifest.summary().clone(),
            identity: manifest.identity().to_owned(),
            pages: (0..count)
                .map(|i| {
                    manifest
                        .page_url(i as usize)
                        .map_err(|_| MangaTransferError::Changed)
                })
                .collect::<Result<_, _>>()?,
        };
        snapshot.check()?;
        Ok(snapshot)
    }
    fn check(&self) -> Result<(), MangaTransferError> {
        if self.pages.len() != self.summary.page_count as usize {
            return Err(MangaTransferError::Changed);
        }
        let mut digest = Sha256::new();
        for value in
            std::iter::once(self.summary.hash.as_str()).chain(self.pages.iter().map(|url| {
                url.path_segments()
                    .and_then(|mut p| p.next_back())
                    .unwrap_or("")
            }))
        {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        let identity: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if identity != self.identity {
            return Err(MangaTransferError::Changed);
        }
        Ok(())
    }
    fn page_url(&self, index: usize) -> Result<reqwest::Url, MangaTransferError> {
        self.pages
            .get(index)
            .cloned()
            .ok_or(MangaTransferError::Changed)
    }
}

struct Options {
    page_bytes: u64,
    total_bytes: u64,
    timeout: Duration,
    #[cfg(test)]
    fixture: Option<SocketAddr>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            page_bytes: PAGE_BYTES,
            total_bytes: TOTAL_BYTES,
            timeout: TRANSFER_TIME,
            #[cfg(test)]
            fixture: None,
        }
    }
}

struct CancelOnDrop(Option<oneshot::Sender<()>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn start(
    manifest: &MangaDexManifest,
    output: &File,
    staging_parent: &File,
    options: Options,
) -> Result<(), MangaTransferError> {
    let manifest = Snapshot::new(manifest)?;
    let output = output
        .try_clone()
        .map_err(|_| MangaTransferError::LocalReview)?;
    let staging_parent = staging_parent
        .try_clone()
        .map_err(|_| MangaTransferError::LocalReview)?;
    let (sender, receiver) = oneshot::channel();
    let mut cancellation = CancelOnDrop(Some(sender));
    // This task retains its output/temp descriptors until cancellation cleanup ends.
    let task = tokio::spawn(supervise(
        manifest,
        output,
        staging_parent,
        options,
        receiver,
    ));
    let result = task.await.map_err(|_| MangaTransferError::LocalReview)?;
    cancellation.0.take();
    result
}

struct Output {
    file: File,
    complete: bool,
}
impl Output {
    fn new(file: File) -> Result<Self, MangaTransferError> {
        let metadata = file
            .metadata()
            .map_err(|_| MangaTransferError::LocalReview)?;
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if !metadata.is_file()
            || metadata.len() != 0
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || flags < 0
            || flags & libc::O_ACCMODE == libc::O_RDONLY
            || flags & libc::O_APPEND != 0
        {
            return Err(MangaTransferError::LocalReview);
        }
        Ok(Self {
            file,
            complete: false,
        })
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        if !self.complete {
            // No asynchronous output writes survive this guard.
            let _ = self.file.set_len(0);
            let _ = self.file.sync_all();
        }
    }
}

async fn supervise(
    manifest: Snapshot,
    output: File,
    staging_parent: File,
    options: Options,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<(), MangaTransferError> {
    let mut output = Output::new(output)?;
    let deadline = Instant::now() + options.timeout;
    let count = manifest.summary.page_count;
    manifest.check()?;
    let stage = PrivateStage::new(staging_parent)?;
    let names = tokio::select! {
        biased;
        _ = &mut cancelled => return Err(MangaTransferError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => return Err(MangaTransferError::TimedOut),
        result = download_pages(&manifest, &stage, &options) => result?,
    };
    if names.len() != count as usize {
        return Err(MangaTransferError::Changed);
    }
    manifest.check()?;
    package(&stage, &names, deadline, &mut cancelled).await?;
    manifest.check()?;
    let mut archive = stage.open("complete.cbz", libc::O_RDONLY)?;
    let archive_bytes = archive
        .metadata()
        .map_err(|_| MangaTransferError::LocalReview)?
        .len();
    if archive_bytes == 0 || archive_bytes > ARCHIVE_BYTES {
        return Err(MangaTransferError::SizeLimit);
    }
    let mut magic = [0; 4];
    archive
        .read_exact(&mut magic)
        .map_err(|_| MangaTransferError::Package)?;
    if magic != *b"PK\x03\x04" {
        return Err(MangaTransferError::Package);
    }
    archive
        .seek(SeekFrom::Start(0))
        .map_err(|_| MangaTransferError::LocalReview)?;
    output
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| MangaTransferError::LocalReview)?;
    let mut buffer = [0u8; 65536];
    let mut copied = 0;
    loop {
        checkpoint(deadline, &mut cancelled)?;
        let count = archive
            .read(&mut buffer)
            .map_err(|_| MangaTransferError::LocalReview)?;
        if count == 0 {
            break;
        }
        copied += count as u64;
        if copied > archive_bytes {
            return Err(MangaTransferError::Changed);
        }
        // Synchronous bounded writes ensure cancellation cannot leave a detached
        // filesystem write that races the output guard's truncation.
        output
            .file
            .write_all(&buffer[..count])
            .map_err(|_| MangaTransferError::LocalReview)?;
        tokio::task::yield_now().await;
    }
    if copied != archive_bytes {
        return Err(MangaTransferError::Changed);
    }
    output
        .file
        .sync_all()
        .map_err(|_| MangaTransferError::LocalReview)?;
    checkpoint(deadline, &mut cancelled)?;
    output.complete = true;
    Ok(())
}

fn checkpoint(
    deadline: Instant,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<(), MangaTransferError> {
    match cancelled.try_recv() {
        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
            return Err(MangaTransferError::Cancelled);
        }
        Err(oneshot::error::TryRecvError::Empty) => {}
    }
    if Instant::now() >= deadline {
        return Err(MangaTransferError::TimedOut);
    }
    Ok(())
}

async fn page_client(url: &reqwest::Url) -> Result<reqwest::Client, MangaTransferError> {
    let host = url.host_str().ok_or(MangaTransferError::NetworkPolicy)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MangaTransferError::NetworkPolicy);
    }
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, 443)),
    )
    .await
    .map_err(|_| MangaTransferError::NetworkPolicy)?
    .map_err(|_| MangaTransferError::NetworkPolicy)?
    .take(17)
    .collect();
    if addresses.is_empty() || addresses.len() > 16 || addresses.iter().any(|a| !public_ip(a.ip()))
    {
        return Err(MangaTransferError::NetworkPolicy);
    }
    client_builder()
        .cookie_store(false)
        .https_only(true)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| MangaTransferError::NetworkPolicy)
}

async fn download_pages(
    manifest: &Snapshot,
    stage: &PrivateStage,
    options: &Options,
) -> Result<Vec<String>, MangaTransferError> {
    let first = manifest
        .page_url(0)
        .map_err(|_| MangaTransferError::Changed)?;
    #[cfg(test)]
    let client = if options.fixture.is_some() {
        client_builder()
            .cookie_store(false)
            .build()
            .map_err(|_| MangaTransferError::NetworkPolicy)?
    } else {
        page_client(&first).await?
    };
    #[cfg(not(test))]
    let client = page_client(&first).await?;
    let mut names = Vec::new();
    let mut total = 0u64;
    for index in 0..manifest.summary.page_count {
        let url = manifest
            .page_url(index as usize)
            .map_err(|_| MangaTransferError::Changed)?;
        if url.origin() != first.origin() {
            return Err(MangaTransferError::NetworkPolicy);
        }
        #[cfg(test)]
        let url = if let Some(address) = options.fixture {
            let mut target = reqwest::Url::parse(&format!("http://{address}/"))
                .map_err(|_| MangaTransferError::NetworkPolicy)?;
            target.set_path(url.path());
            target
        } else {
            url
        };
        let bytes = fetch_page(
            &client,
            url,
            options.page_bytes.min(options.total_bytes - total),
        )
        .await?;
        total += bytes.len() as u64;
        let (bytes, extension) = tokio::task::spawn_blocking(move || {
            let extension = validate_image(&bytes)?;
            Ok::<_, MangaTransferError>((bytes, extension))
        })
        .await
        .map_err(|_| MangaTransferError::InvalidImage)??;
        let name = format!("{:06}.{extension}", index + 1);
        let file = stage.open(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let mut file = tokio::fs::File::from_std(file);
        file.write_all(&bytes)
            .await
            .map_err(|_| MangaTransferError::LocalReview)?;
        file.flush()
            .await
            .map_err(|_| MangaTransferError::LocalReview)?;
        names.push(name);
    }
    Ok(names)
}

async fn fetch_page(
    client: &reqwest::Client,
    url: reqwest::Url,
    limit: u64,
) -> Result<Vec<u8>, MangaTransferError> {
    if limit == 0 {
        return Err(MangaTransferError::SizeLimit);
    }
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| MangaTransferError::Transfer)?;
    if response.status() != reqwest::StatusCode::OK
        || response.headers().contains_key("content-range")
        || response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
    {
        return Err(MangaTransferError::Transfer);
    }
    let length = response.content_length();
    if length.is_some_and(|n| n == 0 || n > limit) {
        return Err(MangaTransferError::SizeLimit);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MangaTransferError::Transfer)?
    {
        if chunk.len() as u64 > limit - bytes.len() as u64 {
            return Err(MangaTransferError::SizeLimit);
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() || length.is_some_and(|n| n != bytes.len() as u64) {
        return Err(MangaTransferError::Transfer);
    }
    Ok(bytes)
}

fn validate_image(bytes: &[u8]) -> Result<&'static str, MangaTransferError> {
    let format = image::guess_format(bytes).map_err(|_| MangaTransferError::InvalidImage)?;
    let extension = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::WebP => "webp",
        _ => return Err(MangaTransferError::InvalidImage),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| MangaTransferError::InvalidImage)?;
    if width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || u64::from(width) * u64::from(height) > 32_000_000
    {
        return Err(MangaTransferError::InvalidImage);
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| MangaTransferError::InvalidImage)?;
    if image.width() != width || image.height() != height {
        return Err(MangaTransferError::InvalidImage);
    }
    Ok(extension)
}

struct PrivateStage {
    parent: File,
    directory: File,
    name: CString,
}
impl PrivateStage {
    fn new(parent: File) -> Result<Self, MangaTransferError> {
        let metadata = parent
            .metadata()
            .map_err(|_| MangaTransferError::LocalReview)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(MangaTransferError::LocalReview);
        }
        let name = CString::new(format!("library-manga-{}", uuid::Uuid::new_v4()))
            .map_err(|_| MangaTransferError::LocalReview)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(MangaTransferError::LocalReview);
        }
        let directory = match open_at(
            parent.as_raw_fd(),
            name.to_str().map_err(|_| MangaTransferError::LocalReview)?,
            libc::O_RDONLY | libc::O_DIRECTORY,
        ) {
            Ok(file) => file,
            Err(error) => {
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
                return Err(error);
            }
        };
        let stage = Self {
            parent,
            directory,
            name,
        };
        let metadata = stage
            .directory
            .metadata()
            .map_err(|_| MangaTransferError::LocalReview)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(MangaTransferError::LocalReview);
        }
        Ok(stage)
    }
    fn path(&self) -> String {
        format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.directory.as_raw_fd()
        )
    }
    fn open(&self, name: &str, flags: i32) -> Result<File, MangaTransferError> {
        open_at(self.directory.as_raw_fd(), name, flags)
    }
}
impl Drop for PrivateStage {
    fn drop(&mut self) {
        // Only our private flat directory; never recurse or follow a symlink.
        if let Ok(entries) = std::fs::read_dir(self.path()) {
            for entry in entries.flatten() {
                use std::os::unix::ffi::OsStrExt;
                if let Ok(name) = CString::new(entry.file_name().as_bytes()) {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
                    }
                }
            }
        }
        unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            );
        }
    }
}
fn open_at(parent: i32, name: &str, flags: i32) -> Result<File, MangaTransferError> {
    let name = CString::new(name).map_err(|_| MangaTransferError::LocalReview)?;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(MangaTransferError::LocalReview);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

async fn package(
    stage: &PrivateStage,
    names: &[String],
    deadline: Instant,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<(), MangaTransferError> {
    checkpoint(deadline, cancelled)?;
    let mut command = tokio::process::Command::new("7z");
    command
        .args([
            "a", "-tzip", "-mx=0", "-mmt=1", "-bd", "-bb0", "-bso0", "-bsp0", "-bse2",
        ])
        .arg(format!("{}/complete.cbz", stage.path()))
        .args(names)
        .current_dir(stage.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    // Child-only resource limits, inherited by any descendants. No shell or URL arguments.
    unsafe {
        command.pre_exec(|| {
            for (resource, value) in [
                (libc::RLIMIT_AS, 512 * 1024 * 1024),
                (libc::RLIMIT_CPU, 60),
                (libc::RLIMIT_FSIZE, ARCHIVE_BYTES),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: value as libc::rlim_t,
                    rlim_max: value as libc::rlim_t,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let package_deadline = deadline.min(Instant::now() + PACKAGE_TIME);
    let child = command.spawn().map_err(|_| MangaTransferError::Package)?;
    supervise_child(child, package_deadline, cancelled).await
}

async fn supervise_child(
    mut child: tokio::process::Child,
    package_deadline: Instant,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<(), MangaTransferError> {
    let process_group = child.id();
    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    let result = tokio::select! {
        biased;
        _ = &mut *cancelled => Err(MangaTransferError::Cancelled),
        _ = tokio::time::sleep_until(package_deadline) => Err(MangaTransferError::TimedOut),
        result = async {
            let (status, (), ()) = tokio::try_join!(
                async { child.wait().await.map_err(|_| MangaTransferError::Package) },
                drain(stdout), drain(stderr),
            )?;
            if !status.success() { return Err(MangaTransferError::Package); }
            Ok(())
        } => result,
    };
    if result.is_err() {
        if let Some(pid) = process_group {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}

async fn drain(mut pipe: impl AsyncRead + Unpin) -> Result<(), MangaTransferError> {
    let mut buffer = [0; 4096];
    let mut total = 0;
    loop {
        let count = pipe
            .read(&mut buffer)
            .await
            .map_err(|_| MangaTransferError::Package)?;
        if count == 0 {
            return Ok(());
        }
        total += count as u64;
        if total > PIPE_BYTES {
            return Err(MangaTransferError::Package);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        HttpLimits, ProviderConfig,
        mangadex_chapters::{MangaDexChapter, MangaDexChapters},
    };
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{Request, Response},
    };
    use serde_json::json;
    use std::{collections::VecDeque, sync::Arc};
    use tokio::sync::Mutex;

    const ID: &str = "22222222-2222-4222-8222-222222222222";
    const HASH: &str = "0123456789abcdef0123456789abcdef";

    struct Reply {
        status: u16,
        bytes: Vec<u8>,
        delay: Duration,
        location: Option<String>,
    }
    impl Reply {
        fn page(bytes: Vec<u8>) -> Self {
            Self {
                status: 200,
                bytes,
                delay: Duration::ZERO,
                location: None,
            }
        }
    }
    #[derive(Clone)]
    struct MockState {
        replies: Arc<Mutex<VecDeque<Reply>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }
    struct Mock {
        address: SocketAddr,
        state: MockState,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Mock {
        async fn new(pages: Vec<Reply>) -> Self {
            let manifest = json!({"result":"ok","baseUrl":"https://node.mangadex.network/private-token",
                "chapter":{"hash":HASH,"data":["first.png","second.png"],"dataSaver":["first.jpg","second.jpg"]}});
            let state = MockState {
                replies: Arc::new(Mutex::new(
                    std::iter::once(Reply::page(manifest.to_string().into_bytes()))
                        .chain(pages)
                        .collect(),
                )),
                requests: Arc::new(Mutex::new(Vec::new())),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new().fallback(handler).with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                address,
                state,
                task,
            }
        }
        async fn manifest(&self) -> MangaDexManifest {
            let adapter = MangaDexChapters::new(
                ProviderConfig::new(
                    &format!("http://{}/", self.address),
                    None,
                    HttpLimits::default(),
                )
                .unwrap(),
            )
            .unwrap();
            adapter
                .at_home(&MangaDexChapter {
                    id: ID.into(),
                    manga_id: "11111111-1111-4111-8111-111111111111".into(),
                    language: "en".into(),
                    chapter: Some("1.5".into()),
                    volume: None,
                    title: None,
                    scanlation_groups: vec![],
                    external_url: None,
                    page_count: 2,
                    version: 1,
                    is_unavailable: false,
                })
                .await
                .unwrap()
        }
    }
    async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
        assert_eq!(request.method(), "GET");
        assert!(!request.headers().contains_key("cookie"));
        state.requests.lock().await.push(request.uri().to_string());
        let reply = state
            .replies
            .lock()
            .await
            .pop_front()
            .expect("unexpected fixture request");
        tokio::time::sleep(reply.delay).await;
        let mut response = Response::builder().status(reply.status);
        if let Some(location) = reply.location {
            response = response.header("location", location);
        }
        response.body(Body::from(reply.bytes)).unwrap()
    }
    fn png() -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            3,
            image::Rgba([1, 2, 3, 255]),
        ))
        .write_to(&mut buffer, ImageFormat::Png)
        .unwrap();
        buffer.into_inner()
    }

    fn staging_parent() -> (tempfile::TempDir, File) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let descriptor = File::open(directory.path()).unwrap();
        (directory, descriptor)
    }

    #[test]
    fn private_stage_uses_parent_descriptor_and_cleans_only_its_child() {
        let (scratch, parent) = staging_parent();
        std::fs::write(scratch.path().join("filepartial"), b"keep").unwrap();
        let stage = PrivateStage::new(parent.try_clone().unwrap()).unwrap();
        let child_name = stage.name.to_str().unwrap();
        let child = scratch.path().join(child_name);
        assert!(child.is_dir());
        assert_eq!(stage.directory.metadata().unwrap().mode() & 0o7777, 0o700);
        let mut page = stage
            .open("000001.png", libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
            .unwrap();
        page.write_all(b"scratch").unwrap();
        drop(page);
        drop(stage);
        assert!(!child.exists());
        assert_eq!(
            std::fs::read(scratch.path().join("filepartial")).unwrap(),
            b"keep"
        );
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
    }

    #[test]
    fn private_stage_rejects_non_directory_or_non_private_parent() {
        use std::os::unix::fs::PermissionsExt;
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            PrivateStage::new(file.as_file().try_clone().unwrap()),
            Err(MangaTransferError::LocalReview)
        ));
        let (scratch, parent) = staging_parent();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            PrivateStage::new(parent),
            Err(MangaTransferError::LocalReview)
        ));
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn packages_complete_pages_into_open_descriptor_with_numeric_names() {
        let png = png();
        let mock = Mock::new(vec![Reply::page(png.clone()), Reply::page(png)]).await;
        let manifest = mock.manifest().await;
        let output = tempfile::Builder::new().suffix(".cbz").tempfile().unwrap();
        let (scratch, parent) = staging_parent();
        transfer_fixture(&manifest, output.as_file(), &parent, mock.address)
            .await
            .unwrap();
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        let decoder = crate::reader::archive::ArchiveDecoder::new();
        let archive = decoder.manifest(output.path()).await.unwrap();
        assert_eq!(
            archive
                .pages
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["000001.png", "000002.png"]
        );
        for page in archive.pages {
            let decoded = decoder.page(output.path(), &page.name).await.unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 3));
        }
        let requests = mock.state.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1], format!("/private-token/data/{HASH}/first.png"));
        assert_eq!(
            requests[2],
            format!("/private-token/data/{HASH}/second.png")
        );
        let archive_bytes = std::fs::read(output.path()).unwrap();
        for secret in [b"private-token".as_slice(), b"mangadex.network".as_slice()] {
            assert!(
                !archive_bytes
                    .windows(secret.len())
                    .any(|window| window == secret)
            );
        }
    }

    #[tokio::test]
    async fn missing_malformed_and_oversized_pages_leave_no_archive() {
        for kind in [
            "missing",
            "malformed",
            "truncated",
            "page_limit",
            "total_limit",
            "redirect",
        ] {
            let png = png();
            let mut bad = Reply::page(png.clone());
            match kind {
                "missing" => bad.status = 404,
                "malformed" => bad.bytes = b"<html>not an image</html>".to_vec(),
                "truncated" => bad.bytes.truncate(png.len() / 2),
                "page_limit" => bad.bytes = vec![0; 1025],
                "redirect" => {
                    bad.status = 302;
                    bad.location = Some("http://127.0.0.1:1/never".into());
                }
                "total_limit" => {}
                _ => unreachable!(),
            }
            let mock = Mock::new(vec![Reply::page(png.clone()), bad]).await;
            let manifest = mock.manifest().await;
            let output = tempfile::NamedTempFile::new().unwrap();
            let (scratch, parent) = staging_parent();
            let result = start(
                &manifest,
                output.as_file(),
                &parent,
                Options {
                    fixture: Some(mock.address),
                    page_bytes: 1024,
                    total_bytes: if kind == "total_limit" {
                        2 * png.len() as u64 - 1
                    } else {
                        TOTAL_BYTES
                    },
                    ..Options::default()
                },
            )
            .await;
            assert_eq!(
                result.unwrap_err(),
                match kind {
                    "missing" | "redirect" => MangaTransferError::Transfer,
                    "malformed" | "truncated" => MangaTransferError::InvalidImage,
                    _ => MangaTransferError::SizeLimit,
                },
                "{kind}"
            );
            assert_eq!(output.as_file().metadata().unwrap().len(), 0, "{kind}");
            assert_eq!(
                std::fs::read_dir(scratch.path()).unwrap().count(),
                0,
                "{kind}"
            );
            assert_eq!(mock.state.requests.lock().await.len(), 3, "{kind}");
        }
    }

    #[tokio::test]
    async fn deadline_and_cancellation_leave_no_finished_archive() {
        for cancel in [false, true] {
            let mut slow = Reply::page(png());
            slow.delay = Duration::from_secs(5);
            let mock = Mock::new(vec![slow]).await;
            let manifest = mock.manifest().await;
            let output = tempfile::NamedTempFile::new().unwrap();
            let (scratch, parent) = staging_parent();
            if cancel {
                let snapshot = Snapshot::new(&manifest).unwrap();
                let (sender, receiver) = oneshot::channel();
                let task = tokio::spawn(supervise(
                    snapshot,
                    output.as_file().try_clone().unwrap(),
                    parent.try_clone().unwrap(),
                    Options {
                        fixture: Some(mock.address),
                        ..Options::default()
                    },
                    receiver,
                ));
                sender.send(()).unwrap();
                assert_eq!(task.await.unwrap(), Err(MangaTransferError::Cancelled));
            } else {
                assert_eq!(
                    start(
                        &manifest,
                        output.as_file(),
                        &parent,
                        Options {
                            fixture: Some(mock.address),
                            timeout: Duration::from_millis(25),
                            ..Options::default()
                        }
                    )
                    .await,
                    Err(MangaTransferError::TimedOut)
                );
            }
            assert_eq!(output.as_file().metadata().unwrap().len(), 0);
            assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn altered_snapshot_and_nonempty_output_are_rejected_before_image_get() {
        let mock = Mock::new(vec![]).await;
        let manifest = mock.manifest().await;
        let mut snapshot = Snapshot::new(&manifest).unwrap();
        snapshot.pages.reverse();
        assert_eq!(snapshot.check(), Err(MangaTransferError::Changed));
        let mut output = tempfile::NamedTempFile::new().unwrap();
        let (_scratch, parent) = staging_parent();
        output.write_all(b"existing").unwrap();
        assert_eq!(
            transfer_fixture(&manifest, output.as_file(), &parent, mock.address).await,
            Err(MangaTransferError::LocalReview)
        );
        assert_eq!(std::fs::read(output.path()).unwrap(), b"existing");
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn process_output_drain_is_bounded() {
        assert_eq!(
            drain(Cursor::new(vec![0; PIPE_BYTES as usize + 1])).await,
            Err(MangaTransferError::Package)
        );
    }

    #[tokio::test]
    async fn child_deadline_and_cancel_kill_and_reap_before_returning() {
        for cancel in [false, true] {
            let mut command = tokio::process::Command::new("sleep");
            command
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .kill_on_drop(true);
            let child = command.spawn().unwrap();
            let pid = child.id().unwrap();
            let (sender, mut receiver) = oneshot::channel();
            let deadline = if cancel {
                Instant::now() + Duration::from_secs(60)
            } else {
                Instant::now()
            };
            let _sender = if cancel {
                sender.send(()).unwrap();
                None
            } else {
                Some(sender)
            };
            assert_eq!(
                supervise_child(child, deadline, &mut receiver).await,
                Err(if cancel {
                    MangaTransferError::Cancelled
                } else {
                    MangaTransferError::TimedOut
                })
            );
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
}
