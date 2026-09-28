use super::{
    http::{Http, json},
    *,
};
use serde::Deserialize;

pub struct Sabnzbd {
    http: Http,
    api_key: String,
}
#[derive(Deserialize)]
struct Reply {
    status: bool,
    #[serde(default)]
    nzo_ids: Vec<String>,
}
#[derive(Deserialize)]
struct Slot {
    nzo_id: String,
    #[serde(alias = "category")]
    cat: String,
    status: String,
}
#[derive(Deserialize)]
struct Slots {
    slots: Vec<Slot>,
}
#[derive(Deserialize)]
struct Queue {
    queue: Slots,
}
#[derive(Deserialize)]
struct History {
    history: Slots,
}

impl Sabnzbd {
    pub fn new(config: ClientConfig, api_key: String) -> Result<Self, ClientError> {
        if api_key.is_empty() || api_key.len() > 4096 {
            return Err(ClientError::InvalidConfiguration);
        }
        Ok(Self {
            http: Http::new(config)?,
            api_key,
        })
    }
    async fn call(&self, fields: &[(&str, &str)], mutation: bool) -> Result<Vec<u8>, ClientError> {
        let mut fields = fields.to_vec();
        fields.extend([("apikey", self.api_key.as_str()), ("output", "json")]);
        let bytes = self
            .http
            .send(self.http.request("api", true)?.form(&fields), mutation)
            .await?;
        let envelope: serde_json::Value = json(&bytes).map_err(|error| {
            if mutation {
                ClientError::NeedsReview
            } else {
                error
            }
        })?;
        if envelope.get("error").is_some()
            || envelope.get("status").and_then(serde_json::Value::as_bool) == Some(false)
        {
            return Err(if mutation {
                ClientError::NeedsReview
            } else {
                match envelope.get("error").and_then(serde_json::Value::as_str) {
                    Some("API Key Required" | "API Key Incorrect") => ClientError::Authentication,
                    _ => ClientError::Rejected,
                }
            });
        }
        Ok(bytes)
    }
    pub async fn test_connection(&self) -> Result<ConnectionInfo, ClientError> {
        #[derive(Deserialize)]
        struct Version {
            version: String,
        }
        let value: Version = json(&self.call(&[("mode", "version")], false).await?)?;
        // Version alone does not validate the full management key.
        let _: Queue = json(
            &self
                .call(&[("mode", "queue"), ("limit", "1")], false)
                .await?,
        )?;
        let _: History = json(
            &self
                .call(&[("mode", "history"), ("limit", "1")], false)
                .await?,
        )?;
        version(&value.version)
    }
    pub async fn enqueue(
        &self,
        attempt: &mut SubmissionAttempt,
        payload: AuthorizedPayload,
    ) -> Result<Vec<OwnedJob>, ClientError> {
        attempt.ensure_fresh()?;
        if payload.kind != ClientKind::Sabnzbd {
            return Err(ClientError::InvalidRequest);
        }
        let name = marker(attempt.own_id);
        let form = reqwest::multipart::Form::new()
            .text("mode", "addfile")
            .text("apikey", self.api_key.clone())
            .text("output", "json")
            .text("cat", self.http.config.category.clone())
            .text("nzbname", name)
            .part(
                "nzbfile",
                reqwest::multipart::Part::bytes(payload.bytes).file_name("release.nzb"),
            );
        let request = self.http.request("api", true)?.multipart(form);
        attempt.begin()?;
        let bytes = self.http.send(request, true).await?;
        let reply: Reply = json(&bytes).map_err(|_| ClientError::NeedsReview)?;
        if !reply.status || reply.nzo_ids.is_empty() {
            return Err(ClientError::NeedsReview);
        }
        let mut jobs = Vec::new();
        for id in reply.nzo_ids {
            if jobs.iter().any(|job: &OwnedJob| job.external_id == id) {
                return Err(ClientError::NeedsReview);
            }
            jobs.push(
                self.http
                    .receipt(attempt.own_id, ClientKind::Sabnzbd, id)
                    .map_err(|_| ClientError::NeedsReview)?,
            );
        }
        Ok(jobs)
    }
    async fn locate(&self, job: &OwnedJob) -> Result<(DownloadState, bool), ClientError> {
        self.http.check(job, ClientKind::Sabnzbd)?;
        for history in [false, true] {
            let mode = if history { "history" } else { "queue" };
            let bytes = self
                .call(
                    &[
                        ("mode", mode),
                        ("nzo_ids", &job.external_id),
                        ("limit", "2"),
                    ],
                    false,
                )
                .await?;
            let slots = if history {
                json::<History>(&bytes)?.history.slots
            } else {
                json::<Queue>(&bytes)?.queue.slots
            };
            if slots.len() > 1 {
                return Err(ClientError::InvalidResponse);
            }
            if let Some(slot) = slots.into_iter().next() {
                if slot.nzo_id != job.external_id || slot.cat != job.category {
                    return Err(ClientError::NotOwned);
                }
                return Ok((map_state(&slot.status, history), history));
            }
        }
        Err(ClientError::NotFound)
    }
    pub async fn status(&self, job: &OwnedJob) -> Result<JobStatus, ClientError> {
        Ok(JobStatus {
            job: job.clone(),
            state: self.locate(job).await?.0,
        })
    }
    /// Status for persisted app receipts only; never enumerates arbitrary user jobs.
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
        self.change(job, "pause").await
    }
    pub async fn resume(&self, job: &OwnedJob) -> Result<(), ClientError> {
        self.change(job, "resume").await
    }
    /// Remove the queue/history record while explicitly preserving downloaded files.
    pub async fn remove(&self, job: &OwnedJob) -> Result<(), ClientError> {
        self.change(job, "delete").await
    }
    async fn change(&self, job: &OwnedJob, action: &str) -> Result<(), ClientError> {
        let (state, history) = self.locate(job).await?;
        if history
            && (action != "delete"
                || !matches!(state, DownloadState::Completed | DownloadState::Failed))
        {
            return Err(ClientError::Unsupported);
        }
        let mode = if history { "history" } else { "queue" };
        let bytes = self
            .call(
                &[
                    ("mode", mode),
                    ("name", action),
                    ("value", &job.external_id),
                    ("del_files", "0"),
                ],
                true,
            )
            .await?;
        let reply: Reply = json(&bytes).map_err(|_| ClientError::NeedsReview)?;
        // SAB 5.1.3 history delete returns status only. Confirm disappearance,
        // without repeating a mutation whose response cannot identify its target.
        if history && action == "delete" && reply.status {
            let envelope: serde_json::Value = json(&bytes).map_err(|_| ClientError::NeedsReview)?;
            if envelope.get("nzo_ids").is_none() {
                return match self.locate(job).await {
                    Err(ClientError::NotFound) => Ok(()),
                    _ => Err(ClientError::NeedsReview),
                };
            }
        }
        if !reply.status || reply.nzo_ids != [job.external_id.clone()] {
            return Err(ClientError::NeedsReview);
        }
        Ok(())
    }
}

pub(super) fn map_state(status: &str, history: bool) -> DownloadState {
    match status {
        "Completed" => DownloadState::Completed,
        "Failed" => DownloadState::Failed,
        "Paused" => DownloadState::Paused,
        "Downloading" | "Fetching" => DownloadState::Downloading,
        "Queued" if history => DownloadState::Processing,
        "Queued" | "Propagating" => DownloadState::Queued,
        "QuickCheck" | "Verifying" | "Repairing" | "Extracting" | "Moving" | "Running" => {
            DownloadState::Processing
        }
        _ => DownloadState::Unknown,
    }
}
