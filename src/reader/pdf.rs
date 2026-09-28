use std::{fs::File, io::Cursor, os::fd::AsRawFd, path::PathBuf, sync::Arc, time::Duration};

use image::ImageReader;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Semaphore, oneshot},
};

use super::archive::PageBytes;

const MAX_PNG: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("PDF could not be decoded within the resource limits")]
    Invalid,
    #[error("encrypted PDFs are unsupported")]
    Encrypted,
    #[error("PDF operation timed out")]
    Timeout,
    #[error("PDF operation was cancelled")]
    Cancelled,
}

#[derive(Clone)]
pub struct PdfDecoder {
    workers: Arc<Semaphore>,
}

impl Default for PdfDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfDecoder {
    pub fn new() -> Self {
        Self {
            workers: Arc::new(Semaphore::new(2)),
        }
    }

    pub async fn manifest(&self, file: Arc<File>) -> Result<usize, PdfError> {
        let bytes = self.run(file, None).await?;
        parse_info(&bytes)
    }

    pub async fn page(&self, file: Arc<File>, page: usize) -> Result<PageBytes, PdfError> {
        if page >= 10_000 {
            return Err(PdfError::Invalid);
        }
        let bytes = self.run(file, Some(page)).await?;
        let (width, height) =
            ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png)
                .into_dimensions()
                .map_err(|_| PdfError::Invalid)?;
        if width == 0 || height == 0 || width > 2000 || height > 2000 {
            return Err(PdfError::Invalid);
        }
        Ok(PageBytes {
            bytes,
            content_type: "image/png",
            width,
            height,
        })
    }

    async fn run(&self, file: Arc<File>, page: Option<usize>) -> Result<Vec<u8>, PdfError> {
        let permit = self
            .workers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| PdfError::Cancelled)?;
        let (mut result_tx, result_rx) = oneshot::channel();
        // This owner survives request cancellation until the child has been reaped.
        tokio::spawn(async move {
            let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
            let operation = run_process(&file, page, &mut cancel_rx);
            tokio::pin!(operation);
            let result = tokio::select! {
                result = &mut operation => result,
                _ = result_tx.closed() => { let _ = cancel_tx.send(()); operation.await }
            };
            drop(permit);
            let _ = result_tx.send(result);
        });
        result_rx.await.unwrap_or(Err(PdfError::Cancelled))
    }
}

fn parse_info(bytes: &[u8]) -> Result<usize, PdfError> {
    let text = std::str::from_utf8(bytes).map_err(|_| PdfError::Invalid)?;
    let mut pages = None;
    let mut encrypted = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Pages:") {
            if pages.is_some() {
                return Err(PdfError::Invalid);
            }
            pages = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| PdfError::Invalid)?,
            );
        }
        if let Some(value) = line.strip_prefix("Encrypted:") {
            if encrypted.is_some() {
                return Err(PdfError::Invalid);
            }
            encrypted = Some(value.trim() == "no");
        }
    }
    if encrypted == Some(false) {
        return Err(PdfError::Encrypted);
    }
    if encrypted != Some(true) {
        return Err(PdfError::Invalid);
    }
    pages
        .filter(|count| (1..=10_000).contains(count))
        .ok_or(PdfError::Invalid)
}

struct PrivateDirectory(PathBuf);
impl PrivateDirectory {
    fn new() -> Result<Self, PdfError> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!("library-reader-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| PdfError::Invalid)?;
        Ok(Self(path))
    }
}
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run_process(
    file: &File,
    page: Option<usize>,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<Vec<u8>, PdfError> {
    let directory = PrivateDirectory::new()?;
    let path = format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
    let mut command = Command::new("/usr/bin/prlimit");
    command.args([
        "--as=536870912",
        "--cpu=10",
        "--fsize=0",
        "--core=0",
        "--nofile=64",
        "--",
    ]);
    if let Some(page) = page {
        command
            .arg("/usr/bin/pdftoppm")
            .args(["-png", "-singlefile", "-scale-to", "2000", "-f"])
            .arg((page + 1).to_string())
            .arg("-l")
            .arg((page + 1).to_string());
    } else {
        command.arg("/usr/bin/pdfinfo");
    }
    command
        .arg(path)
        .current_dir(&directory.0)
        .env_clear()
        .env("LC_ALL", "C")
        .env("HOME", &directory.0)
        .env("TMPDIR", &directory.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| PdfError::Invalid)?;
    let stdout = child.stdout.take().ok_or(PdfError::Invalid)?;
    let stderr = child.stderr.take().ok_or(PdfError::Invalid)?;
    let result = {
        let operation = async {
            let (output, _, status) = tokio::try_join!(
                read_limited(stdout, if page.is_some() { MAX_PNG } else { 64 * 1024 }),
                read_limited(stderr, 16 * 1024),
                async { child.wait().await.map_err(|_| PdfError::Invalid) },
            )?;
            if !status.success() {
                return Err(PdfError::Invalid);
            }
            Ok(output)
        };
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(15), operation) => result.unwrap_or(Err(PdfError::Timeout)),
            _ = cancelled => Err(PdfError::Cancelled),
        }
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, PdfError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| PdfError::Invalid)?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > limit {
            return Err(PdfError::Invalid);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_ambiguous_encrypted_and_unbounded_counts() {
        for info in [
            "Pages: 0\nEncrypted: no",
            "Pages: 10001\nEncrypted: no",
            "Pages: 1",
            "Pages: 1\nPages: 2\nEncrypted: no",
            "Pages: 1\nEncrypted: yes",
        ] {
            assert!(parse_info(info.as_bytes()).is_err());
        }
        assert_eq!(parse_info(b"Pages: 10000\nEncrypted: no").unwrap(), 10000);
    }

    #[tokio::test]
    async fn output_limits_reject_overflow_and_cancellation_reaps_pdf() {
        assert!(read_limited(&b"12345"[..], 4).await.is_err());
        assert_eq!(read_limited(&b"1234"[..], 4).await.unwrap(), b"1234");
        let file = File::open("tests/fixtures/single-page.pdf").unwrap();
        let (cancel, mut cancelled) = oneshot::channel();
        cancel.send(()).unwrap();
        assert!(matches!(
            run_process(&file, Some(0), &mut cancelled).await,
            Err(PdfError::Cancelled)
        ));
    }
}
