use super::*;
use crate::store::sqlite::SqliteStore;

async fn intents() -> (tempfile::TempDir, Intents) {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    (directory, Intents::new(store))
}

#[tokio::test]
async fn lost_response_repeat_returns_one_intent_and_job() {
    let (_directory, intents) = intents().await;
    let request = serde_json::json!({"url": "https://example.test/book"});
    let first = intents
        .create("caller", "key", request.clone())
        .await
        .unwrap();
    let second = intents.create("caller", "key", request).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.job_id, second.job_id);
}

#[tokio::test]
async fn callers_and_keys_are_independent_but_body_conflicts() {
    let (_directory, intents) = intents().await;
    let first = intents
        .create("caller-a", "key", serde_json::json!({"url": "a"}))
        .await
        .unwrap();
    let second = intents
        .create("caller-b", "key", serde_json::json!({"url": "a"}))
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert!(matches!(
        intents
            .create("caller-a", "key", serde_json::json!({"url": "b"}))
            .await,
        Err(IntentError::Conflict)
    ));
}
