//! Download adapters. Root wiring and durable submission fencing: see README.md.
mod http;
mod qbittorrent;
mod sabnzbd;

pub use http::{ClientConfig, HttpLimits};
pub use qbittorrent::QBittorrent;
pub use sabnzbd::Sabnzbd;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ClientError {
    #[error("invalid download client configuration")]
    InvalidConfiguration,
    #[error("invalid download request")]
    InvalidRequest,
    #[error("download client authentication failed")]
    Authentication,
    #[error("download client unavailable")]
    Unavailable,
    #[error("download client response exceeded limit")]
    BodyTooLarge,
    #[error("invalid download client response")]
    InvalidResponse,
    #[error("download client rejected request")]
    Rejected,
    #[error("download job is not owned by this application")]
    NotOwned,
    #[error("download job was not found")]
    NotFound,
    #[error("download may have been changed; review required before further action")]
    NeedsReview,
    #[error("operation unsupported for this job or client version")]
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Sabnzbd,
    QBittorrent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Downloading,
    Paused,
    Processing,
    Completed,
    Seeding,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConnectionInfo {
    pub version: String,
}

/// Only restore from the application's persisted receipt, never from an HTTP DTO.
/// This is a trusted internal capability; validation is not proof of provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OwnedJob {
    own_id: Uuid,
    client_id: Uuid,
    kind: ClientKind,
    category: String,
    external_id: String,
}
impl OwnedJob {
    pub fn from_persisted_receipt(
        own_id: Uuid,
        client_id: Uuid,
        kind: ClientKind,
        category: String,
        external_id: String,
    ) -> Result<Self, ClientError> {
        if own_id.is_nil()
            || client_id.is_nil()
            || !label(&category)
            || !external_valid(kind, &external_id)
        {
            return Err(ClientError::InvalidRequest);
        }
        Ok(Self {
            own_id,
            client_id,
            kind,
            category,
            external_id,
        })
    }
    pub fn own_id(&self) -> Uuid {
        self.own_id
    }
    pub fn external_id(&self) -> &str {
        &self.external_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JobStatus {
    pub job: OwnedJob,
    pub state: DownloadState,
}

/// No reset or Clone: an attempted enqueue cannot be replayed on this token.
/// Root must transactionally fence the intent before enqueue; see README.md.
pub struct SubmissionAttempt {
    own_id: Uuid,
    attempted: bool,
}
impl SubmissionAttempt {
    pub fn from_persisted(own_id: Uuid, previously_attempted: bool) -> Result<Self, ClientError> {
        if own_id.is_nil() {
            return Err(ClientError::InvalidRequest);
        }
        Ok(Self {
            own_id,
            attempted: previously_attempted,
        })
    }
    pub fn was_attempted(&self) -> bool {
        self.attempted
    }
    fn ensure_fresh(&self) -> Result<(), ClientError> {
        if self.attempted {
            Err(ClientError::NeedsReview)
        } else {
            Ok(())
        }
    }
    fn begin(&mut self) -> Result<(), ClientError> {
        self.ensure_fresh()?;
        self.attempted = true;
        Ok(())
    }
}

/// Bytes already fetched and authorized by the acquisition layer. No URL support.
/// Torrent hash must be verified against these bytes by that layer.
pub struct AuthorizedPayload {
    bytes: Vec<u8>,
    kind: ClientKind,
    hash: Option<String>,
}
impl AuthorizedPayload {
    pub fn nzb(bytes: Vec<u8>) -> Result<Self, ClientError> {
        Self::new(bytes, ClientKind::Sabnzbd, None)
    }
    pub fn torrent(bytes: Vec<u8>, verified_hash: String) -> Result<Self, ClientError> {
        if !external_valid(ClientKind::QBittorrent, &verified_hash) {
            return Err(ClientError::InvalidRequest);
        }
        Self::new(bytes, ClientKind::QBittorrent, Some(verified_hash))
    }
    fn new(bytes: Vec<u8>, kind: ClientKind, hash: Option<String>) -> Result<Self, ClientError> {
        if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
            return Err(ClientError::InvalidRequest);
        }
        Ok(Self { bytes, kind, hash })
    }
}

pub(crate) fn label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn external_valid(kind: ClientKind, s: &str) -> bool {
    match kind {
        ClientKind::Sabnzbd => {
            s.strip_prefix("SABnzbd_nzo_").is_some_and(label)
                || (s.len() == 36
                    && Uuid::parse_str(s)
                        .is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == s))
        }
        ClientKind::QBittorrent => {
            [40, 64].contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
    }
}
fn marker(id: Uuid) -> String {
    format!("libraryd-{id}")
}
fn version(value: &str) -> Result<ConnectionInfo, ClientError> {
    let value = value.trim().trim_start_matches('v');
    if value.len() > 48
        || !value.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        || !value.contains('.')
    {
        return Err(ClientError::InvalidResponse);
    }
    Ok(ConnectionInfo {
        version: value.to_owned(),
    })
}
