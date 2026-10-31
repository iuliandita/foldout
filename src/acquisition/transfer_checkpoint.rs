//! URL-free response evidence, synced before each durable offset. No automatic retries.
use super::direct::{DirectError, fingerprint_file, hash, identity};
use crate::store::sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
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
    pub total: Option<u64>,
    pub accept_ranges: bool,
    pub content_type: String,
    pub file_identity: String,
    pub offset: u64,
    pub digest: String,
    pub complete: bool,
}
impl Checkpoint {
    pub fn eligible(&self) -> bool {
        self.offset > 0
            && self
                .total
                .is_some_and(|n| self.offset < n || (self.complete && self.offset == n))
            && self.accept_ranges
            && (self.etag.is_some() || self.last_modified.is_some())
    }
    pub async fn check_file(&self, file: &File) -> Result<(), DirectError> {
        let metadata = file.metadata().map_err(|_| DirectError::LocalReview)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(DirectError::LocalReview);
        }
        if metadata.len() != self.offset || identity(file)? != self.file_identity {
            return Err(DirectError::RestartRequired);
        }
        let (size, digest) = fingerprint_file(file).await?;
        if size != self.offset || digest != self.digest {
            return Err(DirectError::RestartRequired);
        }
        Ok(())
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
        let previous = if resume {
            journal
                .ok_or(DirectError::ResumeUnavailable)?
                .load(ordinal)
                .await?
        } else {
            None
        };
        if let Some(previous) = &previous {
            previous.check_file(file).await?;
            if !previous.eligible() && !(image && previous.complete) {
                return Err(DirectError::ResumeUnavailable);
            }
            if previous.origin != origin(&url) {
                return Err(DirectError::RestartRequired);
            }
            if previous.complete {
                return Ok(());
            }
        } else if resume || file.metadata().map_err(|_| DirectError::LocalReview)?.len() != 0 {
            return Err(DirectError::RestartRequired);
        }
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
                        .or(previous.last_modified.as_ref())
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
                || captured.origin != previous.origin
                || captured.etag != previous.etag
                || captured.last_modified != previous.last_modified
                || captured.content_type != previous.content_type
                || !matches!(span, Some((start, end, total)) if start == previous.offset && end.checked_add(1) == Some(total) && Some(total) == previous.total && end >= start && captured.total == Some(total-start))
            {
                return Err(DirectError::RestartRequired);
            }
            captured = previous.clone();
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
        let mut output = file.try_clone().map_err(|_| DirectError::LocalReview)?;
        output
            .seek(SeekFrom::Start(captured.offset))
            .map_err(|_| DirectError::LocalReview)?;
        // Hash a resumed prefix before streaming, outside every database transaction.
        let mut digest = Sha256::new();
        if captured.offset > 0 {
            use std::io::Read;
            output
                .seek(SeekFrom::Start(0))
                .map_err(|_| DirectError::LocalReview)?;
            let mut left = captured.offset;
            let mut buffer = [0; 65536];
            while left > 0 {
                let count = output
                    .read(&mut buffer[..left.min(65536) as usize])
                    .map_err(|_| DirectError::LocalReview)?;
                if count == 0 {
                    return Err(DirectError::RestartRequired);
                }
                digest.update(&buffer[..count]);
                left -= count as u64;
            }
            let actual: String = digest
                .clone()
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if actual != captured.digest {
                return Err(DirectError::RestartRequired);
            }
        }
        if let Some(journal) = journal {
            journal.save(ordinal, &captured).await?;
        }
        while let Some(chunk) = response.chunk().await.map_err(|_| DirectError::Transfer)? {
            let size = captured
                .offset
                .checked_add(chunk.len() as u64)
                .ok_or(DirectError::SizeLimit)?;
            if size > limit || captured.total.is_some_and(|n| size > n) {
                return Err(DirectError::SizeLimit);
            }
            // Synchronous bounded writes cannot outlive cancellation and race recovery.
            output
                .write_all(&chunk)
                .map_err(|_| DirectError::LocalReview)?;
            output.sync_all().map_err(|_| DirectError::LocalReview)?;
            digest.update(&chunk);
            captured.offset = size;
            captured.digest = digest
                .clone()
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if let Some(journal) = journal {
                journal.save(ordinal, &captured).await?;
            }
        }
        if captured.offset == 0 || captured.total.is_some_and(|n| n != captured.offset) {
            return Err(DirectError::Transfer);
        }
        output.sync_all().map_err(|_| DirectError::LocalReview)?;
        if fingerprint_file(file).await? != (captured.offset, captured.digest.clone()) {
            return Err(DirectError::RestartRequired);
        }
        captured.complete = true;
        if let Some(journal) = journal {
            journal.save(ordinal, &captured).await?;
        }
        Ok(())
    }
}
