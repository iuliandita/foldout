use super::*;
use crate::store::sqlite::SqliteStore;

async fn repository() -> (tempfile::TempDir, Jobs) {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    (directory, Jobs::new(store))
}

#[tokio::test]
async fn concurrent_claims_have_one_winner() {
    let (_directory, jobs) = repository().await;
    jobs.enqueue("refresh", "edition:1", serde_json::json!({"page": 1}))
        .await
        .unwrap();
    let first = jobs.clone();
    let second = jobs.clone();
    let (first, second) = tokio::join!(first.claim("worker-a", 30), second.claim("worker-b", 30));
    assert!(first.unwrap().is_some() ^ second.unwrap().is_some());
}

#[tokio::test]
async fn active_semantic_duplicate_returns_the_existing_job() {
    let (_directory, jobs) = repository().await;
    let first = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({"page": 1}))
        .await
        .unwrap();
    let second = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({"page": 1}))
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    assert!(matches!(jobs.list(101).await, Err(JobError::Invalid(_))));
    assert!(matches!(jobs.list(0).await, Err(JobError::Invalid(_))));
}

#[tokio::test]
async fn pagination_orders_timestamp_ties_without_gaps() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let jobs = Jobs::new(store.clone());
    let mut expected = Vec::new();
    for scope in ["edition:1", "edition:2", "edition:3"] {
        expected.push(
            jobs.enqueue("refresh", scope, serde_json::json!({}))
                .await
                .unwrap()
                .id,
        );
    }
    let mut transaction = store.begin_write().await.unwrap();
    sqlx::query("UPDATE jobs SET created_at = 100")
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let first = jobs.list_page(2, None).await.unwrap();
    let second = jobs
        .list_page(2, first.next_cursor.as_deref())
        .await
        .unwrap();
    let ids = first
        .items
        .into_iter()
        .chain(second.items)
        .map(|job| job.id)
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        ids.into_iter().collect::<std::collections::HashSet<_>>(),
        expected.into_iter().collect()
    );
    assert!(matches!(
        jobs.list_page(1, Some("bad")).await,
        Err(JobError::Invalid(_))
    ));
}

#[tokio::test]
async fn kind_scoped_claim_does_not_take_another_kind() {
    let (_directory, jobs) = repository().await;
    jobs.enqueue("other", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    let refresh = jobs
        .enqueue("refresh", "edition:2", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        jobs.claim_kind("worker", 30, "refresh")
            .await
            .unwrap()
            .unwrap()
            .id,
        refresh.id
    );
}

#[tokio::test]
async fn expired_work_requires_review() {
    let (_directory, jobs) = repository().await;
    let job = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    let claimed = jobs.claim("worker", 1).await.unwrap().unwrap();
    assert_eq!(claimed.id, job.id);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(jobs.reconcile_expired().await.unwrap(), 1);
    assert_eq!(
        jobs.get(&job.id).await.unwrap().unwrap().state,
        JobState::NeedsReview
    );
}

#[tokio::test]
async fn stale_heartbeat_and_completion_are_rejected() {
    let (_directory, jobs) = repository().await;
    let job = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    jobs.claim("worker", 1).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(matches!(
        jobs.heartbeat(&job.id, "worker", 30).await,
        Err(JobError::Conflict)
    ));
    assert!(matches!(
        jobs.complete(&job.id, "worker").await,
        Err(JobError::Conflict)
    ));
}

#[tokio::test]
async fn lease_at_the_current_second_is_expired() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let jobs = Jobs::new(store.clone());
    let job = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    jobs.claim("worker", 30).await.unwrap();
    let mut transaction = store.begin_write().await.unwrap();
    sqlx::query(
        "UPDATE jobs SET lease_until = CAST(strftime('%s', 'now') AS INTEGER) WHERE id = ?",
    )
    .bind(&job.id)
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert!(matches!(
        jobs.heartbeat(&job.id, "worker", 30).await,
        Err(JobError::Conflict)
    ));
    assert_eq!(jobs.reconcile_expired().await.unwrap(), 1);
}

#[tokio::test]
async fn cancellation_and_retries_follow_the_state_machine() {
    let (_directory, jobs) = repository().await;
    let queued = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        jobs.request_cancel(&queued.id).await.unwrap().state,
        JobState::Canceled
    );

    let running = jobs
        .enqueue("refresh", "edition:2", serde_json::json!({}))
        .await
        .unwrap();
    jobs.claim("worker", 30).await.unwrap();
    assert_eq!(
        jobs.request_cancel(&running.id).await.unwrap().state,
        JobState::CancelRequested
    );
    assert_eq!(
        jobs.acknowledge_cancel(&running.id, "worker")
            .await
            .unwrap()
            .state,
        JobState::Canceled
    );

    let retry = jobs
        .enqueue("refresh", "edition:3", serde_json::json!({}))
        .await
        .unwrap();
    jobs.claim("worker", 30).await.unwrap();
    assert_eq!(
        jobs.fail(&retry.id, "worker", "temporary", Some(0))
            .await
            .unwrap()
            .state,
        JobState::RetryWait
    );
}

#[tokio::test]
async fn retry_reviewed_preserves_attempt_history() {
    let (_directory, jobs) = repository().await;
    let job = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    jobs.claim("worker", 30).await.unwrap();
    let failed = jobs
        .fail(&job.id, "worker", "permanent", None)
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    let retried = jobs.retry_reviewed(&job.id).await.unwrap();
    assert_eq!(retried.state, JobState::Queued);
    assert_eq!(retried.attempts, failed.attempts);
}

#[test]
fn retry_delay_is_bounded_and_accepts_deterministic_jitter() {
    assert_eq!(retry_at(100, 1, 0), 101);
    assert_eq!(retry_at(100, 5, 1_000), 124);
}

#[tokio::test]
async fn generic_retry_cannot_reset_acquisition_review_state() {
    let (_directory, jobs) = repository().await;
    let job = jobs
        .enqueue(
            "acquisition.pipeline",
            "acquisition:test",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    jobs.claim("worker", 30).await.unwrap();
    jobs.fail(&job.id, "worker", "uncertain_submission", None)
        .await
        .unwrap();
    assert!(matches!(
        jobs.retry_reviewed(&job.id).await,
        Err(JobError::Conflict)
    ));
    assert_eq!(
        jobs.get(&job.id).await.unwrap().unwrap().state,
        JobState::Failed
    );
}

#[tokio::test]
async fn settings_writes_complete_while_claiming() {
    let (directory, jobs) = repository().await;
    let store = SqliteStore::open(directory.path()).await.unwrap();
    jobs.enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    let claims = jobs.clone();
    let writes = async move {
        for value in 0..20 {
            let mut transaction = store.begin_write().await.unwrap();
            sqlx::query("INSERT INTO service_settings (name, value) VALUES ('jobs-test', ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value")
                .bind(value.to_string()).execute(&mut *transaction).await.unwrap();
            transaction.commit().await.unwrap();
        }
    };
    let claim = async move { claims.claim("worker", 30).await.unwrap() };
    let (_, result) = tokio::join!(writes, claim);
    assert!(result.is_some());
}

#[tokio::test]
async fn reopen_retains_job_state() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let jobs = Jobs::new(store.clone());
    let job = jobs
        .enqueue("refresh", "edition:1", serde_json::json!({}))
        .await
        .unwrap();
    store.close().await;
    let reopened = Jobs::new(SqliteStore::open(directory.path()).await.unwrap());
    assert_eq!(
        reopened.get(&job.id).await.unwrap().unwrap().state,
        JobState::Queued
    );
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
