use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::store::sqlite::SqliteStore;

use super::retry::MAX_CLAIMS;

const MAX_LABEL_BYTES: usize = 128;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("invalid job: {0}")]
    Invalid(&'static str),
    #[error("job state conflicts with this operation")]
    Conflict,
    #[error("job was not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    RetryWait,
    NeedsReview,
    Completed,
    Failed,
    CancelRequested,
    Canceled,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::RetryWait => "retry_wait",
            Self::NeedsReview => "needs_review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::CancelRequested => "cancel_requested",
            Self::Canceled => "canceled",
        }
    }
}

impl FromStr for JobState {
    type Err = JobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "retry_wait" => Ok(Self::RetryWait),
            "needs_review" => Ok(Self::NeedsReview),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancel_requested" => Ok(Self::CancelRequested),
            "canceled" => Ok(Self::Canceled),
            _ => Err(JobError::Database(sqlx::Error::Protocol(
                "invalid job state".into(),
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub scope: String,
    pub payload: serde_json::Value,
    pub payload_version: i64,
    pub state: JobState,
    pub attempts: i64,
    pub worker: Option<String>,
    pub lease_until: Option<i64>,
    pub retry_at: Option<i64>,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobPage {
    pub items: Vec<Job>,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct Jobs {
    store: SqliteStore,
}

impl Jobs {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn enqueue(
        &self,
        kind: &str,
        scope: &str,
        payload: serde_json::Value,
    ) -> Result<Job, JobError> {
        let mut transaction = self.store.begin_write().await?;
        let job = enqueue_in_transaction(&mut transaction, kind, scope, payload).await?;
        transaction.commit().await?;
        Ok(job)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Job>, JobError> {
        fetch_job(self.store.reader(), id).await
    }

    pub async fn list(&self, limit: usize) -> Result<Vec<Job>, JobError> {
        Ok(self.list_page(limit, None).await?.items)
    }

    pub async fn list_page(&self, limit: usize, cursor: Option<&str>) -> Result<JobPage, JobError> {
        if limit == 0 || limit > 100 {
            return Err(JobError::Invalid("limit must be 1-100"));
        }
        let rows = match cursor.map(parse_cursor).transpose()? {
            Some((created_at, id)) => sqlx::query("SELECT id, kind, scope, payload, payload_version, state, attempts, worker, lease_until, retry_at, reason, created_at, updated_at FROM jobs WHERE created_at < ? OR (created_at = ? AND id < ?) ORDER BY created_at DESC, id DESC LIMIT ?")
                .bind(created_at).bind(created_at).bind(id).bind((limit + 1) as i64).fetch_all(self.store.reader()).await?,
            None => sqlx::query("SELECT id, kind, scope, payload, payload_version, state, attempts, worker, lease_until, retry_at, reason, created_at, updated_at FROM jobs ORDER BY created_at DESC, id DESC LIMIT ?")
                .bind((limit + 1) as i64).fetch_all(self.store.reader()).await?,
        };
        let mut items = rows
            .into_iter()
            .map(job_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if items.len() > limit {
            items.truncate(limit);
            Some(cursor_for(items.last().expect("nonempty page")))
        } else {
            None
        };
        Ok(JobPage { items, next_cursor })
    }

    pub async fn claim(&self, worker: &str, lease_seconds: i64) -> Result<Option<Job>, JobError> {
        self.claim_inner(worker, lease_seconds, None).await
    }

    pub async fn claim_kind(
        &self,
        worker: &str,
        lease_seconds: i64,
        kind: &str,
    ) -> Result<Option<Job>, JobError> {
        validate_label(kind)?;
        self.claim_inner(worker, lease_seconds, Some(kind)).await
    }

    async fn claim_inner(
        &self,
        worker: &str,
        lease_seconds: i64,
        kind: Option<&str>,
    ) -> Result<Option<Job>, JobError> {
        validate_label(worker)?;
        validate_lease(lease_seconds)?;
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let id: Option<String> = match kind {
            Some(kind) => sqlx::query_scalar("SELECT id FROM jobs WHERE kind = ? AND (state = 'queued' OR (state = 'retry_wait' AND retry_at <= ?)) AND attempts - claim_base < ? ORDER BY created_at, id LIMIT 1")
                .bind(kind).bind(now).bind(MAX_CLAIMS).fetch_optional(&mut *transaction).await?,
            None => sqlx::query_scalar("SELECT id FROM jobs WHERE (state = 'queued' OR (state = 'retry_wait' AND retry_at <= ?)) AND attempts - claim_base < ? ORDER BY created_at, id LIMIT 1")
                .bind(now).bind(MAX_CLAIMS).fetch_optional(&mut *transaction).await?,
        };
        let Some(id) = id else {
            transaction.commit().await?;
            return Ok(None);
        };
        let changed = match kind {
            Some(kind) => sqlx::query("UPDATE jobs SET state = 'running', attempts = attempts + 1, worker = ?, lease_until = ?, retry_at = NULL, reason = NULL, updated_at = ? WHERE id = ? AND kind = ? AND (state = 'queued' OR (state = 'retry_wait' AND retry_at <= ?)) AND attempts - claim_base < ?")
                .bind(worker).bind(now + lease_seconds).bind(now).bind(&id).bind(kind).bind(now).bind(MAX_CLAIMS).execute(&mut *transaction).await?.rows_affected(),
            None => sqlx::query("UPDATE jobs SET state = 'running', attempts = attempts + 1, worker = ?, lease_until = ?, retry_at = NULL, reason = NULL, updated_at = ? WHERE id = ? AND (state = 'queued' OR (state = 'retry_wait' AND retry_at <= ?)) AND attempts - claim_base < ?")
                .bind(worker).bind(now + lease_seconds).bind(now).bind(&id).bind(now).bind(MAX_CLAIMS).execute(&mut *transaction).await?.rows_affected(),
        };
        if changed == 0 {
            transaction.commit().await?;
            return Ok(None);
        }
        append_event(
            &mut transaction,
            &id,
            "claim",
            None,
            JobState::Running,
            Some(worker),
            None,
        )
        .await?;
        let job = fetch_job(&mut *transaction, &id)
            .await?
            .expect("claimed job exists");
        transaction.commit().await?;
        Ok(Some(job))
    }

    pub async fn heartbeat(
        &self,
        id: &str,
        worker: &str,
        lease_seconds: i64,
    ) -> Result<Job, JobError> {
        validate_label(worker)?;
        validate_lease(lease_seconds)?;
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let changed = sqlx::query("UPDATE jobs SET lease_until = ?, updated_at = ? WHERE id = ? AND state = 'running' AND worker = ? AND lease_until > ?")
            .bind(now + lease_seconds).bind(now).bind(id).bind(worker).bind(now).execute(&mut *transaction).await?.rows_affected();
        if changed == 0 {
            return state_error(&mut transaction, id).await;
        }
        append_event(
            &mut transaction,
            id,
            "heartbeat",
            Some(JobState::Running),
            JobState::Running,
            Some(worker),
            None,
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }

    pub async fn complete(&self, id: &str, worker: &str) -> Result<Job, JobError> {
        validate_label(worker)?;
        self.transition_running(id, worker, JobState::Completed, "complete", None, None)
            .await
    }

    pub async fn fail(
        &self,
        id: &str,
        worker: &str,
        reason: &str,
        retry_at: Option<i64>,
    ) -> Result<Job, JobError> {
        validate_label(worker)?;
        validate_reason(reason)?;
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let current = fetch_job(&mut *transaction, id)
            .await?
            .ok_or(JobError::NotFound)?;
        if current.state != JobState::Running
            || current.worker.as_deref() != Some(worker)
            || current.lease_until.unwrap_or(0) <= now
        {
            return Err(JobError::Conflict);
        }
        let retryable = retry_at.is_some()
            && current.attempts - claim_base(&mut transaction, id).await? < MAX_CLAIMS;
        let state = if retryable {
            JobState::RetryWait
        } else {
            JobState::Failed
        };
        let changed = sqlx::query("UPDATE jobs SET state = ?, worker = NULL, lease_until = NULL, retry_at = ?, reason = ?, updated_at = ? WHERE id = ? AND state = 'running' AND worker = ? AND lease_until > ?")
            .bind(state.as_str()).bind(if retryable { retry_at } else { None }).bind(reason).bind(now).bind(id).bind(worker).bind(now)
            .execute(&mut *transaction).await?.rows_affected();
        if changed == 0 {
            return Err(JobError::Conflict);
        }
        append_event(
            &mut transaction,
            id,
            "fail",
            Some(JobState::Running),
            state,
            Some(worker),
            Some(reason),
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }

    pub async fn request_cancel(&self, id: &str) -> Result<Job, JobError> {
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let current = fetch_job(&mut *transaction, id)
            .await?
            .ok_or(JobError::NotFound)?;
        let state = match current.state {
            JobState::Queued | JobState::RetryWait => JobState::Canceled,
            JobState::Running => JobState::CancelRequested,
            _ => return Err(JobError::Conflict),
        };
        sqlx::query(
            "UPDATE jobs SET state = ?, retry_at = NULL, updated_at = ? WHERE id = ? AND state = ?",
        )
        .bind(state.as_str())
        .bind(now)
        .bind(id)
        .bind(current.state.as_str())
        .execute(&mut *transaction)
        .await?;
        append_event(
            &mut transaction,
            id,
            "request_cancel",
            Some(current.state),
            state,
            None,
            None,
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }

    pub async fn acknowledge_cancel(&self, id: &str, worker: &str) -> Result<Job, JobError> {
        validate_label(worker)?;
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let changed = sqlx::query("UPDATE jobs SET state = 'canceled', worker = NULL, lease_until = NULL, updated_at = ? WHERE id = ? AND state = 'cancel_requested' AND worker = ? AND lease_until > ?")
            .bind(now).bind(id).bind(worker).bind(now).execute(&mut *transaction).await?.rows_affected();
        if changed == 0 {
            return state_error(&mut transaction, id).await;
        }
        append_event(
            &mut transaction,
            id,
            "acknowledge_cancel",
            Some(JobState::CancelRequested),
            JobState::Canceled,
            Some(worker),
            None,
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }

    pub async fn reconcile_expired(&self) -> Result<u64, JobError> {
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM jobs WHERE state IN ('running', 'cancel_requested') AND lease_until <= ?").bind(now).fetch_all(&mut *transaction).await?;
        for id in &ids {
            sqlx::query("UPDATE jobs SET state = 'needs_review', worker = NULL, lease_until = NULL, reason = 'uncertain_effect', updated_at = ? WHERE id = ? AND state IN ('running', 'cancel_requested') AND lease_until <= ?")
                .bind(now).bind(id).bind(now).execute(&mut *transaction).await?;
            append_event(
                &mut transaction,
                id,
                "lease_expired",
                None,
                JobState::NeedsReview,
                None,
                Some("uncertain_effect"),
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(ids.len() as u64)
    }

    pub async fn retry_reviewed(&self, id: &str) -> Result<Job, JobError> {
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let current = fetch_job(&mut *transaction, id)
            .await?
            .ok_or(JobError::NotFound)?;
        // Acquisition retries must reconcile the persisted submission fence and receipts.
        if current.kind == "acquisition.pipeline" {
            return Err(JobError::Conflict);
        }
        if !matches!(current.state, JobState::NeedsReview | JobState::Failed) {
            return Err(JobError::Conflict);
        }
        sqlx::query("UPDATE jobs SET state = 'queued', claim_base = attempts, worker = NULL, lease_until = NULL, retry_at = NULL, reason = NULL, updated_at = ? WHERE id = ?")
            .bind(now).bind(id).execute(&mut *transaction).await?;
        append_event(
            &mut transaction,
            id,
            "retry_reviewed",
            Some(current.state),
            JobState::Queued,
            None,
            None,
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }

    async fn transition_running(
        &self,
        id: &str,
        worker: &str,
        state: JobState,
        event: &str,
        reason: Option<&str>,
        retry_at: Option<i64>,
    ) -> Result<Job, JobError> {
        let now = unix_seconds();
        let mut transaction = self.store.begin_write().await?;
        let changed = sqlx::query("UPDATE jobs SET state = ?, worker = NULL, lease_until = NULL, retry_at = ?, reason = ?, updated_at = ? WHERE id = ? AND state = 'running' AND worker = ? AND lease_until > ?")
            .bind(state.as_str()).bind(retry_at).bind(reason).bind(now).bind(id).bind(worker).bind(now).execute(&mut *transaction).await?.rows_affected();
        if changed == 0 {
            return state_error(&mut transaction, id).await;
        }
        append_event(
            &mut transaction,
            id,
            event,
            Some(JobState::Running),
            state,
            Some(worker),
            reason,
        )
        .await?;
        let job = fetch_job(&mut *transaction, id)
            .await?
            .expect("updated job exists");
        transaction.commit().await?;
        Ok(job)
    }
}

pub(crate) async fn enqueue_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    kind: &str,
    scope: &str,
    payload: serde_json::Value,
) -> Result<Job, JobError> {
    validate_label(kind)?;
    validate_label(scope)?;
    let payload = canonical_json(&payload)?;
    let fingerprint = fingerprint(&payload);
    if let Some(job) = sqlx::query("SELECT id, kind, scope, payload, payload_version, state, attempts, worker, lease_until, retry_at, reason, created_at, updated_at FROM jobs WHERE kind = ? AND scope = ? AND payload_fingerprint = ? AND state IN ('queued', 'running', 'retry_wait', 'needs_review', 'cancel_requested')")
        .bind(kind).bind(scope).bind(&fingerprint).fetch_optional(&mut **transaction).await?.map(job_from_row).transpose()? { return Ok(job) }
    let now = unix_seconds();
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO jobs (id, kind, scope, payload, payload_version, payload_fingerprint, state, attempts, claim_base, created_at, updated_at) VALUES (?, ?, ?, ?, 1, ?, 'queued', 0, 0, ?, ?)")
        .bind(&id).bind(kind).bind(scope).bind(&payload).bind(&fingerprint).bind(now).bind(now).execute(&mut **transaction).await?;
    append_event(
        transaction,
        &id,
        "enqueue",
        None,
        JobState::Queued,
        None,
        None,
    )
    .await?;
    Ok(fetch_job(&mut **transaction, &id)
        .await?
        .expect("inserted job exists"))
}

pub(crate) fn canonical_json(value: &serde_json::Value) -> Result<String, JobError> {
    let json =
        serde_json::to_string(value).map_err(|_| JobError::Invalid("payload must be JSON"))?;
    if json.len() > MAX_PAYLOAD_BYTES {
        return Err(JobError::Invalid("payload exceeds 64 KiB"));
    }
    Ok(json)
}

pub(crate) fn fingerprint(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}
fn cursor_for(job: &Job) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}:{}", job.created_at, job.id))
}
fn parse_cursor(value: &str) -> Result<(i64, String), JobError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| JobError::Invalid("invalid cursor"))?;
    let text = std::str::from_utf8(&decoded).map_err(|_| JobError::Invalid("invalid cursor"))?;
    let (created_at, id) = text
        .split_once(':')
        .ok_or(JobError::Invalid("invalid cursor"))?;
    if created_at.is_empty() || id.is_empty() || text.matches(':').count() != 1 {
        return Err(JobError::Invalid("invalid cursor"));
    }
    let created_at = created_at
        .parse::<i64>()
        .map_err(|_| JobError::Invalid("invalid cursor"))?;
    let parsed = Uuid::parse_str(id).map_err(|_| JobError::Invalid("invalid cursor"))?;
    if parsed.to_string() != id {
        return Err(JobError::Invalid("invalid cursor"));
    }
    Ok((created_at, id.into()))
}
pub(crate) fn validate_label(value: &str) -> Result<(), JobError> {
    if value.is_empty()
        || value.len() > MAX_LABEL_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'_' | b'-'))
    {
        return Err(JobError::Invalid(
            "labels must be 1-128 ASCII identifier characters",
        ));
    }
    Ok(())
}
fn validate_lease(value: i64) -> Result<(), JobError> {
    if !(1..=super::retry::MAX_LEASE_SECONDS).contains(&value) {
        Err(JobError::Invalid("lease must be 1-300 seconds"))
    } else {
        Ok(())
    }
}
fn validate_reason(value: &str) -> Result<(), JobError> {
    if value.is_empty() || value.len() > 1024 {
        Err(JobError::Invalid("reason must be 1-1024 bytes"))
    } else {
        Ok(())
    }
}
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
async fn claim_base(transaction: &mut Transaction<'_, Sqlite>, id: &str) -> Result<i64, JobError> {
    Ok(
        sqlx::query_scalar("SELECT claim_base FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_one(&mut **transaction)
            .await?,
    )
}
async fn state_error(transaction: &mut Transaction<'_, Sqlite>, id: &str) -> Result<Job, JobError> {
    if fetch_job(&mut **transaction, id).await?.is_some() {
        Err(JobError::Conflict)
    } else {
        Err(JobError::NotFound)
    }
}
async fn append_event(
    transaction: &mut Transaction<'_, Sqlite>,
    id: &str,
    event: &str,
    from: Option<JobState>,
    to: JobState,
    worker: Option<&str>,
    reason: Option<&str>,
) -> Result<(), JobError> {
    let now = unix_seconds();
    sqlx::query("INSERT INTO job_events (job_id, event, from_state, to_state, worker, reason, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(event).bind(from.map(JobState::as_str)).bind(to.as_str()).bind(worker).bind(reason).bind(now).execute(&mut **transaction).await?;
    Ok(())
}
async fn fetch_job<'e, E>(executor: E, id: &str) -> Result<Option<Job>, JobError>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query("SELECT id, kind, scope, payload, payload_version, state, attempts, worker, lease_until, retry_at, reason, created_at, updated_at FROM jobs WHERE id = ?").bind(id).fetch_optional(executor).await?.map(job_from_row).transpose()
}
fn job_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Job, JobError> {
    Ok(Job {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        scope: row.try_get("scope")?,
        payload: serde_json::from_str(&row.try_get::<String, _>("payload")?)
            .map_err(|_| JobError::Database(sqlx::Error::Protocol("invalid job payload".into())))?,
        payload_version: row.try_get("payload_version")?,
        state: row.try_get::<String, _>("state")?.parse()?,
        attempts: row.try_get("attempts")?,
        worker: row.try_get("worker")?,
        lease_until: row.try_get("lease_until")?,
        retry_at: row.try_get("retry_at")?,
        reason: row.try_get("reason")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
