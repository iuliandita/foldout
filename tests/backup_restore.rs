use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use libraryd::{
    backup,
    settings::{CreateIntegration, EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The key loader rejects group/other-accessible state dirs, so do not inherit the caller's umask.
fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

async fn source(path: &Path) -> (SqliteStore, String) {
    private_dir(path);
    let key = EncryptionKey::load_or_create(path).await.unwrap();
    let store = SqliteStore::open(path).await.unwrap();
    let settings = Settings::new(store.clone(), key);
    let input: CreateIntegration = serde_json::from_value(serde_json::json!({
        "kind": "comicvine", "label": "Metadata", "base_url": "https://example.com",
        "enabled": true, "api_key": "backup-secret-do-not-leak"
    }))
    .unwrap();
    let id = settings.create(input).await.unwrap().id;
    (store, id)
}

async fn put(store: &SqliteStore, value: &str) {
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT OR REPLACE INTO service_settings (name, value) VALUES ('backup-test', ?)")
        .bind(value)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn value(store: &SqliteStore) -> String {
    sqlx::query_scalar("SELECT value FROM service_settings WHERE name = 'backup-test'")
        .fetch_one(store.reader())
        .await
        .unwrap()
}

#[tokio::test]
async fn wal_snapshot_roundtrip_authenticates_settings_and_keeps_source_live() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let saved = root.path().join("backup ' quoted");
    let restored = root.path().join("restored");
    let (store, id) = source(&state).await;
    put(&store, "before").await;
    assert!(
        fs::metadata(state.join("library.sqlite3-wal"))
            .unwrap()
            .len()
            > 0
    );
    backup::backup_existing(&state, &saved).await.unwrap();
    put(&store, "after").await;
    backup::restore(&saved, &restored).await.unwrap();
    let recovered = SqliteStore::open(&restored).await.unwrap();
    assert_eq!(value(&recovered).await, "before");
    assert_eq!(value(&store).await, "after");
    let key = EncryptionKey::load_or_create(&restored).await.unwrap();
    let integration = Settings::new(recovered.clone(), key)
        .get(&id)
        .await
        .unwrap();
    assert!(integration.api_key_configured);
    for directory in [&saved, &saved.join("snapshot"), &restored] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for directory in [saved.join("snapshot"), restored.clone()] {
        for name in ["library.sqlite3", "encryption.key"] {
            assert_eq!(
                fs::metadata(directory.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    let bytes = fs::read(saved.join("snapshot/library.sqlite3")).unwrap();
    let secret = b"backup-secret-do-not-leak";
    assert!(!bytes.windows(secret.len()).any(|part| part == secret));
    recovered.close().await;
    store.close().await;
}

#[tokio::test]
async fn refuses_existing_destinations_and_live_restore() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let saved = root.path().join("backup");
    let (store, _) = source(&state).await;
    put(&store, "unchanged").await;
    backup::backup_existing(&state, &saved).await.unwrap();
    assert!(backup::backup(&store, &state, &saved).await.is_err());
    assert!(backup::restore(&saved, &state).await.is_err());
    assert_eq!(value(&store).await, "unchanged");
    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(backup::restore(&saved, &empty).await.is_err());
    assert!(backup::backup(&store, &state, &empty).await.is_err());
    store.close().await;
}

#[tokio::test]
async fn rejects_missing_wrong_and_insecure_keys_without_publishing() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, _) = source(&state).await;
    let key = state.join("encryption.key");
    let bytes = fs::read(&key).unwrap();
    fs::remove_file(&key).unwrap();
    let missing = root.path().join("missing");
    assert!(backup::backup(&store, &state, &missing).await.is_err());
    assert!(!missing.join("snapshot").exists());
    assert!(!key.exists());
    fs::write(&key, [42u8; 32]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let wrong = root.path().join("wrong");
    assert!(backup::backup(&store, &state, &wrong).await.is_err());
    assert!(!wrong.join("snapshot").exists());
    fs::write(&key, bytes).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let insecure = root.path().join("insecure");
    assert!(backup::backup(&store, &state, &insecure).await.is_err());
    assert!(!insecure.join("snapshot").exists());
    store.close().await;
}

#[tokio::test]
async fn restore_rejects_key_substitution_and_database_corruption() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, _) = source(&state).await;
    for corrupt_key in [true, false] {
        let saved = root.path().join(format!("backup-{corrupt_key}"));
        backup::backup(&store, &state, &saved).await.unwrap();
        let name = if corrupt_key {
            "encryption.key"
        } else {
            "library.sqlite3"
        };
        fs::write(saved.join("snapshot").join(name), [17u8; 32]).unwrap();
        assert!(
            backup::restore(&saved, &root.path().join(format!("restore-{corrupt_key}")))
                .await
                .is_err()
        );
    }
    store.close().await;
}

#[tokio::test]
async fn allows_keyless_empty_settings_but_rejects_wrong_store_and_partial_backup() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    private_dir(&state);
    let store = SqliteStore::open(&state).await.unwrap();
    let saved = root.path().join("backup");
    backup::backup_existing(&state, &saved).await.unwrap();
    let restored = root.path().join("restored");
    backup::restore(&saved, &restored).await.unwrap();
    assert!(!restored.join("encryption.key").exists());
    private_dir(&root.path().join("other"));
    let other = SqliteStore::open(&root.path().join("other")).await.unwrap();
    assert!(
        backup::backup(&other, &state, &root.path().join("wrong-store"))
            .await
            .is_err()
    );
    let partial = root.path().join("partial");
    fs::create_dir(&partial).unwrap();
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        backup::restore(&partial, &root.path().join("partial-restore"))
            .await
            .is_err()
    );
    other.close().await;
    store.close().await;
}

#[tokio::test]
async fn rejects_symlink_key_and_newer_schema() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, _) = source(&state).await;
    fs::rename(state.join("encryption.key"), state.join("real.key")).unwrap();
    std::os::unix::fs::symlink("real.key", state.join("encryption.key")).unwrap();
    assert!(
        backup::backup(&store, &state, &root.path().join("symlink"))
            .await
            .is_err()
    );
    fs::remove_file(state.join("encryption.key")).unwrap();
    fs::rename(state.join("real.key"), state.join("encryption.key")).unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET version = 999999 WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let saved = root.path().join("future");
    assert!(backup::backup(&store, &state, &saved).await.is_err());
    assert!(!saved.join("snapshot").exists());
    store.close().await;
}

#[tokio::test]
async fn snapshot_excludes_an_open_writer_transaction() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, _) = source(&state).await;
    put(&store, "committed").await;
    let mut writer = store.begin_write().await.unwrap();
    sqlx::query("UPDATE service_settings SET value = 'uncommitted' WHERE name = 'backup-test'")
        .execute(&mut *writer)
        .await
        .unwrap();
    let saved = root.path().join("backup");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backup::backup(&store, &state, &saved),
    )
    .await
    .unwrap()
    .unwrap();
    writer.commit().await.unwrap();
    let restored = root.path().join("restored");
    backup::restore(&saved, &restored).await.unwrap();
    let recovered = SqliteStore::open(&restored).await.unwrap();
    assert_eq!(value(&recovered).await, "committed");
    assert_eq!(value(&store).await, "uncommitted");
    recovered.close().await;
    store.close().await;
}

#[tokio::test]
async fn restore_rechecks_migration_version_even_when_checksums_match() {
    use sha2::{Digest, Sha256};

    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, _) = source(&state).await;
    let saved = root.path().join("backup");
    backup::backup(&store, &state, &saved).await.unwrap();
    let snapshot = saved.join("snapshot");
    let modified = SqliteStore::open(&snapshot).await.unwrap();
    let mut tx = modified.begin_write().await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET version = 999999 WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    modified.close().await;
    let hash: String = Sha256::digest(fs::read(snapshot.join("library.sqlite3")).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let manifest_path = snapshot.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["database_sha256"] = hash.into();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let result = backup::restore(&saved, &root.path().join("restored")).await;
    assert!(matches!(
        result,
        Err(backup::BackupError::Store(
            libraryd::store::sqlite::StoreError::NewerSchema
        ))
    ));
    store.close().await;
}

#[tokio::test]
async fn backup_preserves_old_encrypted_schema_until_fresh_restore() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let saved = root.path().join("backup");
    let restored = root.path().join("restored");
    historical_state(&state, 13, true).await;
    let source_database = state.join("library.sqlite3");
    let source_bytes = fs::read(&source_database).unwrap();
    backup::backup_existing(&state, &saved).await.unwrap();
    let snapshot_database = saved.join("snapshot/library.sqlite3");
    let snapshot_bytes = fs::read(&snapshot_database).unwrap();
    assert_eq!(newest_migration(&source_database).await, 13);
    assert_eq!(newest_migration(&snapshot_database).await, 13);
    assert_eq!(
        schema_objects(&source_database).await,
        schema_objects(&snapshot_database).await
    );
    assert_eq!(historical_value(&snapshot_database).await, "kept");
    backup::restore(&saved, &restored).await.unwrap();
    assert!(newest_migration(&restored.join("library.sqlite3")).await > 13);
    assert_eq!(newest_migration(&source_database).await, 13);
    assert_eq!(fs::read(&source_database).unwrap(), source_bytes);
    assert_eq!(fs::read(&snapshot_database).unwrap(), snapshot_bytes);
    assert_eq!(
        historical_value(&restored.join("library.sqlite3")).await,
        "kept"
    );
    let recovered = SqliteStore::open(&restored).await.unwrap();
    let key = EncryptionKey::load_or_create(&restored).await.unwrap();
    assert!(Settings::new(recovered.clone(), key).list().await.unwrap()[0].api_key_configured);
    recovered.close().await;
}

#[tokio::test]
async fn cli_backup_preserves_pre7_keyless_source() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let saved = root.path().join("backup");
    historical_state(&state, 6, false).await;
    let database = state.join("library.sqlite3");
    let before = fs::read(&database).unwrap();
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_libraryd"))
        .arg("backup")
        .arg(&saved)
        .env_clear()
        .env("LIBRARY_STATE_DIR", &state)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(
        newest_migration(&saved.join("snapshot/library.sqlite3")).await,
        6
    );
    assert_eq!(
        historical_value(&saved.join("snapshot/library.sqlite3")).await,
        "kept"
    );
    backup::restore(&saved, &root.path().join("restored"))
        .await
        .unwrap();
    assert!(newest_migration(&root.path().join("restored/library.sqlite3")).await > 6);
    for directory in [
        &state,
        &saved.join("snapshot"),
        &root.path().join("restored"),
    ] {
        assert!(!directory.join("encryption.key").exists());
    }
    assert_eq!(
        historical_value(&root.path().join("restored/library.sqlite3")).await,
        "kept"
    );
}

async fn historical_state(path: &Path, version: i64, encrypted: bool) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    let database = path.join("library.sqlite3");
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&database)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    let migrator = sqlx::migrate::Migrator::with_migrations(
        MIGRATOR
            .iter()
            .filter(|item| item.version <= version)
            .cloned()
            .collect(),
    );
    migrator.run(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO service_settings(name,value) VALUES('historical','kept')")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    if encrypted {
        let fixture = tempfile::tempdir().unwrap();
        let (store, _) = source(fixture.path()).await;
        let row = sqlx::query("SELECT id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext FROM integrations").fetch_one(store.reader()).await.unwrap();
        store.close().await;
        fs::copy(
            fixture.path().join("encryption.key"),
            path.join("encryption.key"),
        )
        .unwrap();
        fs::set_permissions(
            path.join("encryption.key"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&database))
                .await
                .unwrap();
        sqlx::query("INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(row.get::<String,_>("id")).bind(row.get::<String,_>("kind")).bind(row.get::<String,_>("label")).bind(row.get::<String,_>("base_url")).bind(row.get::<bool,_>("enabled")).bind(row.get::<String,_>("options")).bind(row.get::<i64,_>("secret_version")).bind(row.get::<Vec<u8>,_>("secret_nonce")).bind(row.get::<Vec<u8>,_>("secret_ciphertext")).execute(&mut connection).await.unwrap();
        connection.close().await.unwrap();
    }
}

async fn newest_migration(path: &Path) -> i64 {
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let version: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    version.unwrap()
}

async fn historical_value(path: &Path) -> String {
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let value = sqlx::query_scalar("SELECT value FROM service_settings WHERE name = 'historical'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    value
}

async fn schema_objects(path: &Path) -> Vec<(String, String, String)> {
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let objects = sqlx::query(
        "SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY type,name",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.get("type"), row.get("name"), row.get("sql")))
    .collect();
    connection.close().await.unwrap();
    objects
}
