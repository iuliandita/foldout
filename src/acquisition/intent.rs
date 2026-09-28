use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::jobs::store::{canonical_json, enqueue_in_transaction, fingerprint, validate_label};
use crate::jobs::{JobError, Jobs};
use crate::store::sqlite::SqliteStore;

#[derive(Debug, Error)]
pub enum IntentError {
    #[error("invalid intent: {0}")]
    Invalid(&'static str),
    #[error("idempotency key was reused with a different request")]
    Conflict,
    #[error("intent was not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub caller: String,
    pub key: String,
    pub request: serde_json::Value,
    pub job_id: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct Intents {
    store: SqliteStore,
    jobs: Jobs,
}

impl Intents {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            jobs: Jobs::new(store.clone()),
            store,
        }
    }

    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    pub async fn create(
        &self,
        caller: &str,
        key: &str,
        request: serde_json::Value,
    ) -> Result<Intent, IntentError> {
        validate_label(caller).map_err(intent_invalid)?;
        validate_label(key).map_err(intent_invalid)?;
        let request_text = canonical_json(&request).map_err(intent_invalid)?;
        let request_fingerprint = fingerprint(&request_text);
        let mut transaction = self.store.begin_write().await?;
        if let Some(intent) = find_intent(&mut *transaction, caller, key).await? {
            transaction.commit().await?;
            return if intent.request_fingerprint == request_fingerprint {
                Ok(intent.intent)
            } else {
                Err(IntentError::Conflict)
            };
        }
        let job = enqueue_in_transaction(
            &mut transaction,
            "acquisition.intent",
            &format!("acquisition:{caller}"),
            request.clone(),
        )
        .await
        .map_err(job_error)?;
        let intent = Intent {
            id: Uuid::new_v4().to_string(),
            caller: caller.into(),
            key: key.into(),
            request,
            job_id: job.id,
            created_at: now(),
        };
        sqlx::query("INSERT INTO acquisition_intents (id, caller, idempotency_key, request, request_fingerprint, job_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&intent.id).bind(&intent.caller).bind(&intent.key).bind(&request_text).bind(&request_fingerprint).bind(&intent.job_id).bind(intent.created_at).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(intent)
    }
}

struct StoredIntent {
    intent: Intent,
    request_fingerprint: String,
}
async fn find_intent<'e, E>(
    executor: E,
    caller: &str,
    key: &str,
) -> Result<Option<StoredIntent>, IntentError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query("SELECT id, caller, idempotency_key, request, request_fingerprint, job_id, created_at FROM acquisition_intents WHERE caller = ? AND idempotency_key = ?").bind(caller).bind(key).fetch_optional(executor).await?;
    row.map(|row| {
        Ok(StoredIntent {
            request_fingerprint: row.try_get("request_fingerprint")?,
            intent: Intent {
                id: row.try_get("id")?,
                caller: row.try_get("caller")?,
                key: row.try_get("idempotency_key")?,
                request: serde_json::from_str(&row.try_get::<String, _>("request")?).map_err(
                    |_| {
                        IntentError::Database(sqlx::Error::Protocol(
                            "invalid intent request".into(),
                        ))
                    },
                )?,
                job_id: row.try_get("job_id")?,
                created_at: row.try_get("created_at")?,
            },
        })
    })
    .transpose()
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn intent_invalid(error: JobError) -> IntentError {
    match error {
        JobError::Invalid(message) => IntentError::Invalid(message),
        JobError::Database(error) => IntentError::Database(error),
        JobError::Conflict | JobError::NotFound => IntentError::Invalid("invalid request"),
    }
}
fn job_error(error: JobError) -> IntentError {
    intent_invalid(error)
}
