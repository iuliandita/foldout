use libraryd::{
    settings::{EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{collections::BTreeMap, ffi::OsString, fs, path::Path, time::Duration};

fn snapshot(directory: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn missing_key_stops_production_startup_without_database_changes() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    SqliteStore::ensure_state_directory(&state).await.unwrap();
    let key = EncryptionKey::load_or_create(&state).await.unwrap();
    let store = SqliteStore::open(&state).await.unwrap();
    let settings = Settings::new(store.clone(), key);
    settings
        .create(
            serde_json::from_value(json!({
                "kind": "comicvine",
                "label": "Saved credentials",
                "base_url": "https://example.invalid/api/",
                "enabled": false,
                "api_key": "test-startup-secret"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    drop(settings);
    store.close().await;

    // A restored database may use DELETE mode; opening the writer would change it to WAL.
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(state.join("library.sqlite3")),
    )
    .await
    .unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode = DELETE")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(mode, "delete");
    connection.close().await.unwrap();
    fs::rename(
        state.join("encryption.key"),
        parent.path().join("saved.key"),
    )
    .unwrap();
    let before = snapshot(&state);

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_libraryd"))
            .arg("serve")
            .env_clear()
            .env("LIBRARY_STATE_DIR", &state)
            .env("LIBRARY_LISTEN", "127.0.0.1:0")
            .env("LIBRARY_ORIGIN", "http://127.0.0.1:8787")
            .env("LIBRARY_WORKERS", "false")
            .env("TOKIO_WORKER_THREADS", "2")
            .env("RUST_LOG", "libraryd=error")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("production startup did not exit within 15 seconds")
    .unwrap();
    assert!(!output.status.success());
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logs.contains("encryption.key is missing"), "{logs}");
    assert!(!state.join("encryption.key").exists());
    assert!(!state.join("setup-token").exists());
    assert!(
        snapshot(&state) == before,
        "startup changed the state files"
    );
}
