use std::fs;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row};
use tempfile::tempdir;

use crate::store::sqlite::SqliteStore;

#[cfg(unix)]
#[tokio::test]
async fn rejects_insecure_state_without_creating_or_changing_database() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempdir().unwrap();
    let database = directory.path().join("library.sqlite3");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SqliteStore::open(directory.path()).await.is_err());
    assert!(!database.exists());
    fs::write(&database, b"untouched database").unwrap();
    assert!(SqliteStore::open(directory.path()).await.is_err());
    assert_eq!(fs::read(database).unwrap(), b"untouched database");
    assert_eq!(
        fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_state_without_writing_to_target() {
    use std::os::unix::fs::symlink;
    let parent = tempdir().unwrap();
    let target = tempdir().unwrap();
    let link = parent.path().join("state");
    symlink(target.path(), &link).unwrap();
    assert!(SqliteStore::open(&link).await.is_err());
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn reopen_preserves_service_settings() {
    let state_dir = private_directory();
    let store = SqliteStore::open(state_dir.path()).await.unwrap();
    let mut transaction = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO service_settings (name, value) VALUES ('theme', 'dark')")
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    store.close().await;

    let store = SqliteStore::open(state_dir.path()).await.unwrap();
    let value: String = sqlx::query("SELECT value FROM service_settings WHERE name = 'theme'")
        .fetch_one(store.reader())
        .await
        .unwrap()
        .get("value");

    assert_eq!(value, "dark");
    store.close().await;
}

#[tokio::test]
async fn enables_foreign_keys_for_each_pool_connection() {
    let state_dir = private_directory();
    let store = SqliteStore::open(state_dir.path()).await.unwrap();

    let (mut first, mut second, mut third, mut fourth) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(
                store.reader().acquire(),
                store.reader().acquire(),
                store.reader().acquire(),
                store.reader().acquire(),
            )
        })
        .await
        .expect("reader pool should provide four connections")
        .unwrap();
    for connection in [&mut first, &mut second, &mut third, &mut fourth] {
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut **connection)
            .await
            .unwrap();
        assert_eq!(enabled, 1);
    }
    drop((first, second, third, fourth));

    let mut transaction = store.begin_write().await.unwrap();
    let enabled: i64 = sqlx::query("PRAGMA foreign_keys")
        .fetch_one(&mut *transaction)
        .await
        .unwrap()
        .get(0);
    transaction.rollback().await.unwrap();

    assert_eq!(enabled, 1);
    store.close().await;
}

#[tokio::test]
async fn reader_pool_is_readonly_and_database_uses_wal_with_full_synchronous() {
    let state_dir = private_directory();
    let store = SqliteStore::open(state_dir.path()).await.unwrap();
    let mut transaction = store.begin_write().await.unwrap();
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    transaction.rollback().await.unwrap();

    let error =
        match sqlx::query("INSERT INTO service_settings (name, value) VALUES ('readonly', 'no')")
            .execute(store.reader())
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("reader pool should reject writes"),
        };

    assert_eq!(journal_mode, "wal");
    assert_eq!(synchronous, 2);
    assert!(error.to_string().contains("readonly"));
    store.close().await;
}

#[tokio::test]
async fn reports_context_when_state_directory_is_a_file() {
    let state_dir = tempdir().unwrap();
    let state_file = state_dir.path().join("not-a-directory");
    fs::write(&state_file, "file").unwrap();

    let error = match SqliteStore::open(&state_file).await {
        Err(error) => error,
        Ok(_) => panic!("a file cannot be used as a state directory"),
    };

    assert_eq!(error.to_string(), "cannot create or access state directory");
}

#[tokio::test]
async fn rejects_newer_schema_without_changing_existing_database() {
    let state_dir = private_directory();
    let database_path = state_dir.path().join("library.sqlite3");
    let mut connection = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TEXT NOT NULL, success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (999, 'future', '2026-01-01', 1, X'00', 0)")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let before = fs::read(&database_path).unwrap();

    let error = match SqliteStore::open(state_dir.path()).await {
        Err(error) => error,
        Ok(_) => panic!("a newer schema should be rejected"),
    };
    let after = fs::read(&database_path).unwrap();

    assert!(error.to_string().contains("newer"));
    assert_eq!(after, before);
}

#[tokio::test]
async fn settings_writes_complete_while_readers_are_active() {
    let state_dir = private_directory();
    let store = SqliteStore::open(state_dir.path()).await.unwrap();

    let writes = async {
        for value in 0..20 {
            let mut transaction = store.begin_write().await.unwrap();
            sqlx::query("INSERT INTO service_settings (name, value) VALUES ('concurrency-test', ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value")
                .bind(value.to_string())
                .execute(&mut *transaction)
                .await
                .unwrap();
            transaction.commit().await.unwrap();
        }
    };
    let reads = async {
        for _ in 0..100 {
            sqlx::query("SELECT value FROM service_settings WHERE name = 'concurrency-test'")
                .fetch_optional(store.reader())
                .await
                .unwrap();
        }
    };

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(writes, reads);
    })
    .await
    .expect("settings writes and reads should not deadlock");
    store.close().await;
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
