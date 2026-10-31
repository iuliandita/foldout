//! URL-free response evidence, synced before each durable offset. No automatic retries.
use super::direct::{DirectError, hash, identity};
use crate::store::sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
};

#[derive(Clone)]
pub(crate) struct Journal {
    pub store: SqliteStore,
    pub acquisition_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    pub origin: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    #[serde(default)]
    pub last_modified_strong: bool,
    pub total: Option<u64>,
    pub accept_ranges: bool,
    pub content_type: String,
    pub file_identity: String,
    pub offset: u64,
    pub digest: String,
    pub complete: bool,
}
impl Checkpoint {
    pub fn unstarted(&self) -> bool {
        !self.complete && self.offset == 0
    }
    pub fn eligible(&self) -> bool {
        self.offset > 0
            && self
                .total
                .is_some_and(|n| self.offset < n || (self.complete && self.offset == n))
            && self.accept_ranges
            && (self.etag.is_some() || (self.last_modified.is_some() && self.last_modified_strong))
    }
    pub fn check_metadata(&self, file: &File) -> Result<(), DirectError> {
        check_private(file)?;
        let metadata = file.metadata().map_err(|_| DirectError::LocalReview)?;
        if metadata.len() < self.offset || identity(file)? != self.file_identity {
            return Err(DirectError::RestartRequired);
        }
        Ok(())
    }
    async fn verify(&self, file: &File) -> Result<Writer, DirectError> {
        self.check_metadata(file)?;
        let mut file = file.try_clone().map_err(|_| DirectError::LocalReview)?;
        let expected = self.clone();
        blocking(move || {
            check_private(&file)?;
            let before = file.metadata().map_err(|_| DirectError::LocalReview)?;
            file.seek(SeekFrom::Start(0))
                .map_err(|_| DirectError::LocalReview)?;
            let mut digest = Sha256::new();
            let mut left = expected.offset;
            let mut buffer = [0; 65536];
            while left > 0 {
                let count = file
                    .read(&mut buffer[..left.min(65536) as usize])
                    .map_err(|_| DirectError::LocalReview)?;
                if count == 0 {
                    return Err(DirectError::RestartRequired);
                }
                digest.update(&buffer[..count]);
                left -= count as u64;
            }
            if hex_digest(&digest) != expected.digest {
                return Err(DirectError::RestartRequired);
            }
            let after = file.metadata().map_err(|_| DirectError::LocalReview)?;
            check_private(&file)?;
            if before.len() != after.len()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
            {
                return Err(DirectError::RestartRequired);
            }
            // Only a verified prefix authorizes removal of a crash/cancellation tail.
            if file.metadata().map_err(|_| DirectError::LocalReview)?.len() > expected.offset {
                file.set_len(expected.offset)
                    .map_err(|_| DirectError::LocalReview)?;
                file.sync_all().map_err(|_| DirectError::LocalReview)?;
            }
            Ok(Writer {
                file,
                digest,
                offset: expected.offset,
                private: true,
            })
        })
        .await
    }
}
impl Journal {
    pub async fn load(&self, ordinal: u32) -> Result<Option<Checkpoint>, DirectError> {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT checkpoint FROM direct_transfer_parts WHERE acquisition_id=? AND ordinal=?",
        )
        .bind(&self.acquisition_id)
        .bind(ordinal)
        .fetch_optional(self.store.reader())
        .await?
        .ok_or(DirectError::RestartRequired)?;
        value
            .map(|s| serde_json::from_str(&s).map_err(|_| DirectError::RestartRequired))
            .transpose()
    }
    async fn save(&self, ordinal: u32, checkpoint: &Checkpoint) -> Result<(), DirectError> {
        #[cfg(test)]
        CHECKPOINT_TRACE.lock().unwrap().push((
            false,
            checkpoint.file_identity.clone(),
            checkpoint.offset,
        ));
        let value = serde_json::to_string(checkpoint).map_err(|_| DirectError::Database)?;
        // All writes, syncs and hashing have already finished. The writer has one connection.
        let mut tx = self.store.begin_write().await?;
        let changed = sqlx::query("UPDATE direct_transfer_parts SET checkpoint=? WHERE acquisition_id=? AND ordinal=? AND EXISTS(SELECT 1 FROM direct_acquisitions WHERE id=? AND state='downloading' AND attempted=1)")
            .bind(value).bind(&self.acquisition_id).bind(ordinal).bind(&self.acquisition_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DirectError::Conflict);
        }
        tx.commit().await?;
        Ok(())
    }
}
fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() || value.len() > 1024 {
        return None;
    }
    Some(value.to_owned())
}
fn etag(value: String) -> Option<String> {
    (value.starts_with('"')
        && value.ends_with('"')
        && value.len() >= 2
        && value[1..value.len() - 1]
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b)))
    .then_some(value)
}
fn response_checkpoint(
    url: &reqwest::Url,
    response: &reqwest::Response,
    file: &File,
) -> Result<Checkpoint, DirectError> {
    let h = response.headers();
    Ok(Checkpoint {
        origin: origin(url),
        etag: header(h, "etag").and_then(etag),
        last_modified: header(h, "last-modified").filter(|s| valid_http_date(s)),
        last_modified_strong: strong_modified(h),
        total: response.content_length(),
        accept_ranges: header(h, "accept-ranges").is_some_and(|s| s.eq_ignore_ascii_case("bytes")),
        content_type: header(h, "content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
        file_identity: identity(file)?,
        offset: 0,
        digest: hash(b""),
        complete: false,
    })
}
fn valid_http_date(value: &str) -> bool {
    const FORMAT: &str = "%a, %d %b %Y %H:%M:%S GMT";
    value.len() == 29
        && jiff::civil::DateTime::strptime(FORMAT, value)
            .is_ok_and(|date| date.strftime(FORMAT).to_string() == value)
}
fn strong_modified(headers: &reqwest::header::HeaderMap) -> bool {
    let parse = |name| {
        header(headers, name)
            .filter(|s| valid_http_date(s))
            .and_then(|s| jiff::civil::DateTime::strptime("%a, %d %b %Y %H:%M:%S GMT", s).ok())
            .and_then(|date| date.to_zoned(jiff::tz::TimeZone::UTC).ok())
            .map(|date| date.timestamp().as_second())
    };
    match (parse("date"), parse("last-modified")) {
        (Some(date), Some(modified)) => date
            .checked_sub(modified)
            .is_some_and(|seconds| seconds >= 1),
        _ => false,
    }
}

fn check_private(file: &File) -> Result<(), DirectError> {
    let m = file.metadata().map_err(|_| DirectError::LocalReview)?;
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
    {
        return Err(DirectError::LocalReview);
    }
    Ok(())
}
fn hex_digest(digest: &Sha256) -> String {
    digest
        .clone()
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
// Cancellation must drain an already-started blocking operation before worker locks are released.
// Ordinary execution waits asynchronously; only cancellation can wait in Drop.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, DirectError> + Send + 'static,
) -> Result<T, DirectError> {
    struct Drain(Option<std::sync::mpsc::Receiver<()>>);
    impl Drop for Drain {
        fn drop(&mut self) {
            if let Some(done) = self.0.take() {
                let _ = done.recv();
            }
        }
    }
    let (done, completed) = std::sync::mpsc::channel();
    let task = tokio::task::spawn_blocking(move || {
        let _done = done;
        work()
    });
    let mut drain = Drain(Some(completed));
    let result = task.await.map_err(|_| DirectError::LocalReview)?;
    drain.0.take();
    result
}
struct Writer {
    file: File,
    digest: Sha256,
    offset: u64,
    private: bool,
}
impl Writer {
    async fn flush(
        mut self,
        bytes: Vec<u8>,
        mut captured: Checkpoint,
        complete: bool,
    ) -> Result<(Self, Checkpoint), DirectError> {
        blocking(move || {
            if self.private {
                check_private(&self.file)?;
            }
            self.file
                .seek(SeekFrom::Start(self.offset))
                .map_err(|_| DirectError::LocalReview)?;
            self.file
                .write_all(&bytes)
                .map_err(|_| DirectError::LocalReview)?;
            self.digest.update(&bytes);
            self.offset += bytes.len() as u64;
            self.file.sync_all().map_err(|_| DirectError::LocalReview)?;
            // This value can only leave the blocking job after sync_all has succeeded.
            #[cfg(test)]
            CHECKPOINT_TRACE.lock().unwrap().push((
                true,
                captured.file_identity.clone(),
                self.offset,
            ));
            captured.offset = self.offset;
            captured.digest = hex_digest(&self.digest);
            captured.complete = complete;
            Ok((self, captured))
        })
        .await
    }
}
pub(crate) async fn verified_page(
    file: &File,
    checkpoint: &Checkpoint,
    limit: u64,
) -> Result<Vec<u8>, DirectError> {
    checkpoint.check_metadata(file)?;
    if !checkpoint.complete || checkpoint.offset == 0 || checkpoint.offset > limit {
        return Err(DirectError::RestartRequired);
    }
    let mut file = file.try_clone().map_err(|_| DirectError::LocalReview)?;
    let checkpoint = checkpoint.clone();
    blocking(move || {
        let before = file.metadata().map_err(|_| DirectError::LocalReview)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| DirectError::LocalReview)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(checkpoint.offset)
            .read_to_end(&mut bytes)
            .map_err(|_| DirectError::LocalReview)?;
        let after = file.metadata().map_err(|_| DirectError::LocalReview)?;
        if bytes.len() as u64 != checkpoint.offset
            || hash(&bytes) != checkpoint.digest
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
        {
            return Err(DirectError::RestartRequired);
        }
        check_private(&file)?;
        if after.len() > checkpoint.offset {
            file.set_len(checkpoint.offset)
                .map_err(|_| DirectError::LocalReview)?;
            file.sync_all().map_err(|_| DirectError::LocalReview)?;
        }
        Ok(bytes)
    })
    .await
}
const CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;
const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) fn origin(url: &reqwest::Url) -> String {
    hash(url.origin().ascii_serialization().as_bytes())
}
fn range(value: &str) -> Option<(u64, u64, u64)> {
    let (span, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

pub(crate) struct Transfer<'a> {
    pub journal: Option<&'a Journal>,
    pub ordinal: u32,
    pub resume: bool,
    pub image: bool,
}
impl Transfer<'_> {
    pub(crate) async fn run(
        &self,
        client: reqwest::Client,
        url: reqwest::Url,
        file: &File,
        limit: u64,
    ) -> Result<(), DirectError> {
        let Self {
            journal,
            ordinal,
            resume,
            image,
        } = *self;
        let mut previous = if resume {
            journal
                .ok_or(DirectError::ResumeUnavailable)?
                .load(ordinal)
                .await?
        } else {
            None
        };
        if journal.is_some() {
            check_private(file)?;
        }
        if image && previous.as_ref().is_some_and(Checkpoint::unstarted) {
            let checkpoint = previous.take().ok_or(DirectError::RestartRequired)?;
            if file.metadata().map_err(|_| DirectError::LocalReview)?.len() > 0 {
                // A header-only checkpoint owns no reusable bytes. Verify ownership and
                // its empty prefix before discarding an uncommitted staging tail.
                checkpoint.verify(file).await?;
            }
        }
        let mut writer = if let Some(previous) = &previous {
            if image && previous.complete {
                // Packaging reads and verifies this complete page once on the blocking pool.
                previous.check_metadata(file)?;
                return Ok(());
            }
            let writer = previous.verify(file).await?;
            if !previous.eligible() && !(image && previous.complete) {
                return Err(DirectError::ResumeUnavailable);
            }
            if !image && previous.origin != origin(&url) {
                return Err(DirectError::RestartRequired);
            }
            if previous.complete {
                return Ok(());
            }
            writer
        } else {
            if (resume && !image)
                || file.metadata().map_err(|_| DirectError::LocalReview)?.len() != 0
            {
                return Err(DirectError::RestartRequired);
            }
            Writer {
                file: file.try_clone().map_err(|_| DirectError::LocalReview)?,
                digest: Sha256::new(),
                offset: 0,
                private: journal.is_some(),
            }
        };
        let mut request = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(previous) = &previous {
            request = request
                .header("range", format!("bytes={}-", previous.offset))
                .header(
                    "if-range",
                    previous
                        .etag
                        .as_ref()
                        .or(previous
                            .last_modified
                            .as_ref()
                            .filter(|_| previous.last_modified_strong))
                        .ok_or(DirectError::ResumeUnavailable)?,
                );
        }
        let mut response = request.send().await.map_err(|_| DirectError::Transfer)?;
        if response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
        {
            return Err(crate::providers::ProviderError::ChallengeRequired.into());
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(crate::providers::ProviderError::RateLimited {
                retry_after_seconds: None,
            }
            .into());
        }
        let mut captured = response_checkpoint(&url, &response, file)?;
        if let Some(previous) = &previous {
            let span = header(response.headers(), "content-range").and_then(|s| range(&s));
            if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
                || (!image && captured.origin != previous.origin)
                || captured.etag != previous.etag
                || captured.last_modified != previous.last_modified
                || captured.content_type != previous.content_type
                || !matches!(span, Some((start, end, total)) if start == previous.offset && end.checked_add(1) == Some(total) && Some(total) == previous.total && end >= start && captured.total == Some(total-start))
            {
                return Err(DirectError::RestartRequired);
            }
            captured = previous.clone();
            captured.origin = origin(&url);
        } else if response.status() != reqwest::StatusCode::OK
            || response.headers().contains_key("content-range")
        {
            return Err(DirectError::Transfer);
        }
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
        {
            return Err(if resume {
                DirectError::RestartRequired
            } else {
                DirectError::InvalidArchive
            });
        }
        if !if image {
            matches!(
                captured.content_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp" | "application/octet-stream"
            )
        } else {
            matches!(
                captured.content_type.as_str(),
                "application/octet-stream"
                    | "application/zip"
                    | "application/x-zip-compressed"
                    | "application/x-cbz"
                    | "application/vnd.comicbook+zip"
                    | "application/x-rar-compressed"
                    | "application/vnd.rar"
                    | "application/x-cbr"
                    | "application/vnd.comicbook-rar"
            )
        } {
            return Err(DirectError::InvalidArchive);
        }
        if captured.total.is_some_and(|n| n == 0 || n > limit) {
            return Err(DirectError::SizeLimit);
        }
        if let Some(journal) = journal {
            journal.save(ordinal, &captured).await?;
        }
        let mut pending = Vec::new();
        let mut timer = tokio::time::interval(CHECKPOINT_INTERVAL);
        timer.tick().await;
        loop {
            let next = tokio::select! {
                result = response.chunk() => Some(result),
                _ = timer.tick(), if !pending.is_empty() => None,
            };
            let (complete, failure) = match next {
                Some(Ok(Some(chunk))) => {
                    let size = writer
                        .offset
                        .checked_add(pending.len() as u64)
                        .and_then(|n| n.checked_add(chunk.len() as u64))
                        .ok_or(DirectError::SizeLimit)?;
                    if size > limit || captured.total.is_some_and(|n| size > n) {
                        return Err(DirectError::SizeLimit);
                    }
                    pending.extend_from_slice(&chunk);
                    if pending.len() < CHECKPOINT_BYTES {
                        continue;
                    }
                    (false, None)
                }
                Some(Ok(None)) => {
                    let size = writer.offset + pending.len() as u64;
                    let valid = size > 0 && captured.total.is_none_or(|n| n == size);
                    (valid, (!valid).then_some(DirectError::Transfer))
                }
                Some(Err(_)) => (false, Some(DirectError::Transfer)),
                None => (false, None),
            };
            (writer, captured) = writer
                .flush(std::mem::take(&mut pending), captured, complete)
                .await?;
            if let Some(journal) = journal {
                journal.save(ordinal, &captured).await?;
            }
            timer.reset();
            if let Some(error) = failure {
                return Err(error);
            }
            if complete {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
pub(crate) static CHECKPOINT_TRACE: std::sync::Mutex<Vec<(bool, String, u64)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_modified_requires_an_older_date_at_second_precision() {
        for (date, expected) in [
            (None, false),
            (Some("Wed, 21 Oct 2026 09:59:59 GMT"), false),
            (Some("Wed, 21 Oct 2026 10:00:00 GMT"), false),
            (Some("Wed, 21 Oct 2026 10:00:01 GMT"), true),
            (Some("Fri, 23 Oct 2026 10:00:00 GMT"), true),
            (Some("invalid"), false),
        ] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "last-modified",
                "Wed, 21 Oct 2026 10:00:00 GMT".parse().unwrap(),
            );
            if let Some(date) = date {
                headers.insert("date", date.parse().unwrap());
            }
            assert_eq!(strong_modified(&headers), expected, "{date:?}");
        }
    }
}
