use super::{
    http::{Http, json},
    *,
};
use serde::Deserialize;

pub struct QBittorrent {
    http: Http,
    username: String,
    password: String,
}
#[derive(Deserialize)]
struct Torrent {
    hash: String,
    category: String,
    tags: String,
    state: String,
}

#[derive(Deserialize)]
struct AddReceipt {
    success_count: u64,
    failure_count: u64,
    pending_count: u64,
    // This adapter submits one torrent; reject empty, duplicate, or extra IDs.
    added_torrent_ids: [String; 1],
}

impl QBittorrent {
    pub fn new(
        config: ClientConfig,
        username: String,
        password: String,
    ) -> Result<Self, ClientError> {
        if username.is_empty()
            || password.is_empty()
            || username.len() > 4096
            || password.len() > 4096
        {
            return Err(ClientError::InvalidConfiguration);
        }
        Ok(Self {
            http: Http::new(config)?,
            username,
            password,
        })
    }
    pub async fn test_connection(&self) -> Result<ConnectionInfo, ClientError> {
        let request = self
            .http
            .request("api/v2/auth/login", true)?
            .form(&[("username", &self.username), ("password", &self.password)]);
        let bytes = self.http.send(request, false).await?;
        // 5.2.3: empty 204; older servers: 200 Ok. Cookie names are opaque.
        if !bytes.is_empty() && bytes != b"Ok." {
            return Err(ClientError::Authentication);
        }
        let bytes = self
            .http
            .send(self.http.request("api/v2/app/version", false)?, false)
            .await?;
        let info = version(std::str::from_utf8(&bytes).map_err(|_| ClientError::InvalidResponse)?)?;
        if !info.version.starts_with("4.") && !info.version.starts_with("5.") {
            return Err(ClientError::Unsupported);
        }
        Ok(info)
    }
    async fn torrents(&self, hash: &str) -> Result<Vec<Torrent>, ClientError> {
        let bytes = self
            .http
            .send(
                self.http
                    .request("api/v2/torrents/info", false)?
                    .query(&[("hashes", hash)]),
                false,
            )
            .await?;
        let torrents: Vec<Torrent> = json(&bytes)?;
        if torrents.len() > 1 || torrents.iter().any(|t| t.hash != hash) {
            return Err(ClientError::InvalidResponse);
        }
        Ok(torrents)
    }
    pub async fn enqueue(
        &self,
        attempt: &mut SubmissionAttempt,
        payload: AuthorizedPayload,
    ) -> Result<Vec<OwnedJob>, ClientError> {
        attempt.ensure_fresh()?;
        if payload.kind != ClientKind::QBittorrent {
            return Err(ClientError::InvalidRequest);
        }
        let hash = payload.hash.as_deref().ok_or(ClientError::InvalidRequest)?;
        self.test_connection().await?;
        // Do not claim or recategorize a torrent that already exists, even in our category.
        if !self.torrents(hash).await?.is_empty() {
            attempt.begin()?;
            return Err(ClientError::NeedsReview);
        }
        let tag = marker(attempt.own_id);
        let form = reqwest::multipart::Form::new()
            .text("category", self.http.config.category.clone())
            .text("tags", tag)
            .part(
                "torrents",
                reqwest::multipart::Part::bytes(payload.bytes).file_name("release.torrent"),
            );
        let request = self
            .http
            .request("api/v2/torrents/add", true)?
            .multipart(form);
        attempt.begin()?;
        let bytes = self.http.send(request, true).await.inspect_err(|_| {
            tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "add_http", "qBittorrent submission needs review");
        })?;
        if !bytes.is_empty() && bytes != b"Ok." {
            // 5.2.3 returns a JSON receipt; legacy empty/Ok. responses remain valid.
            if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
                tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "add_response_shape", "qBittorrent submission needs review");
                return Err(ClientError::NeedsReview);
            }
            let receipt: AddReceipt = json(&bytes).map_err(|_| {
                tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "add_json_schema", "qBittorrent submission needs review");
                ClientError::NeedsReview
            })?;
            if receipt.success_count != 1
                || receipt.failure_count != 0
                || receipt.pending_count != 0
                || receipt.added_torrent_ids[0] != hash
            {
                tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "add_receipt_identity", "qBittorrent submission needs review");
                return Err(ClientError::NeedsReview);
            }
        }
        let job = self
            .http
            .receipt(attempt.own_id, ClientKind::QBittorrent, hash.to_owned())
            .map_err(|error| {
                tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "local_receipt", ?error, "qBittorrent submission needs review");
                ClientError::NeedsReview
            })?;
        // qBittorrent adds asynchronously: a valid receipt can precede visibility.
        // Only absence is retried, and only with GETs. The deadline includes HTTP time.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match self.locate(&job).await {
                    Err(ClientError::NotFound) => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    result => return result,
                }
            }
        })
            .await
            .unwrap_or(Err(ClientError::NotFound))
            .map_err(|error| {
                tracing::warn!(target: "libraryd::clients::qbittorrent", stage = "ownership_lookup", ?error, "qBittorrent submission needs review");
                ClientError::NeedsReview
            })?;
        Ok(vec![job])
    }
    async fn locate(&self, job: &OwnedJob) -> Result<DownloadState, ClientError> {
        self.http.check(job, ClientKind::QBittorrent)?;
        let torrent = self
            .torrents(&job.external_id)
            .await?
            .into_iter()
            .next()
            .ok_or(ClientError::NotFound)?;
        let tag = marker(job.own_id);
        if torrent.category != job.category
            || !torrent.tags.split(',').any(|value| value.trim() == tag)
        {
            return Err(ClientError::NotOwned);
        }
        Ok(map_state(&torrent.state))
    }
    pub async fn status(&self, job: &OwnedJob) -> Result<JobStatus, ClientError> {
        self.http.check(job, ClientKind::QBittorrent)?;
        self.test_connection().await?;
        Ok(JobStatus {
            job: job.clone(),
            state: self.locate(job).await?,
        })
    }
    pub async fn queue(&self, jobs: &[OwnedJob]) -> Result<Vec<JobStatus>, ClientError> {
        if jobs.len() > 100 {
            return Err(ClientError::InvalidRequest);
        }
        let mut result = Vec::new();
        for job in jobs {
            result.push(self.status(job).await?);
        }
        Ok(result)
    }
    pub async fn pause(&self, job: &OwnedJob) -> Result<(), ClientError> {
        self.change(job, "stop", "pause").await
    }
    pub async fn resume(&self, job: &OwnedJob) -> Result<(), ClientError> {
        self.change(job, "start", "resume").await
    }
    /// Removes only the torrent record, never the payload files.
    pub async fn remove(&self, job: &OwnedJob) -> Result<(), ClientError> {
        self.change(job, "delete", "delete").await
    }
    async fn change(&self, job: &OwnedJob, current: &str, legacy: &str) -> Result<(), ClientError> {
        self.http.check(job, ClientKind::QBittorrent)?;
        let info = self.test_connection().await?;
        self.locate(job).await?;
        let action = if info.version.starts_with("4.") {
            legacy
        } else {
            current
        };
        let request = self
            .http
            .request(&format!("api/v2/torrents/{action}"), true)?
            .form(&[
                ("hashes", job.external_id.as_str()),
                ("deleteFiles", "false"),
            ]);
        let bytes = self.http.send(request, true).await?;
        if !bytes.is_empty() && bytes != b"Ok." {
            return Err(ClientError::NeedsReview);
        }
        Ok(())
    }
}

pub(super) fn map_state(state: &str) -> DownloadState {
    match state {
        "error" | "missingFiles" => DownloadState::Failed,
        "uploading" | "stalledUP" | "forcedUP" => DownloadState::Seeding,
        "pausedUP" | "stoppedUP" | "queuedUP" => DownloadState::Completed,
        "pausedDL" | "stoppedDL" => DownloadState::Paused,
        "queuedDL" | "allocating" => DownloadState::Queued,
        "downloading" | "stalledDL" | "forcedDL" | "metaDL" | "forcedMetaDL" => {
            DownloadState::Downloading
        }
        "checkingUP" | "checkingDL" | "checkingResumeData" | "moving" => DownloadState::Processing,
        _ => DownloadState::Unknown,
    }
}
