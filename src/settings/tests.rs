use super::*;
use serde_json::json;
use std::time::Duration;

fn input(label: &str) -> CreateIntegration {
    serde_json::from_value(json!({
        "kind": "comicvine", "label": label, "base_url": "https://example.invalid/api/", "enabled": true,
        "api_key": "test-secret-api-key", "username": "test-secret-username", "password": "test-secret-password"
    })).unwrap()
}

async fn service(directory: &std::path::Path) -> (SqliteStore, Settings) {
    let key = EncryptionKey::load_or_create(directory).await.unwrap();
    let store = SqliteStore::open(directory).await.unwrap();
    (store.clone(), Settings::new(store, key))
}

#[tokio::test]
async fn encrypts_database_and_restarts_with_same_key() {
    let directory = temporary_directory();
    let (store, settings) = service(directory.path()).await;
    let integration = settings.create(input("Primary")).await.unwrap();
    let row = sqlx::query(
        "SELECT secret_version, secret_nonce, secret_ciphertext FROM integrations WHERE id = ?",
    )
    .bind(&integration.id)
    .fetch_one(store.reader())
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("secret_version"), 1);
    assert_eq!(row.get::<Vec<u8>, _>("secret_nonce").len(), 24);
    let ciphertext: Vec<u8> = row.get("secret_ciphertext");
    for secret in [
        "test-secret-api-key",
        "test-secret-username",
        "test-secret-password",
    ] {
        assert!(
            !ciphertext
                .windows(secret.len())
                .any(|w| w == secret.as_bytes())
        );
    }
    drop(settings);
    store.close().await;
    for entry in std::fs::read_dir(directory.path()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("library.sqlite3")
        {
            let bytes = std::fs::read(path).unwrap();
            for secret in [
                "test-secret-api-key",
                "test-secret-username",
                "test-secret-password",
            ] {
                assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
            }
        }
    }
    let (_, settings) = service(directory.path()).await;
    let loaded = settings.load_private(&integration.id).await.unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("test-secret-api-key"));
    assert_eq!(loaded.username.as_deref(), Some("test-secret-username"));
    assert_eq!(loaded.password.as_deref(), Some("test-secret-password"));
    assert!(matches!(
        settings.adapter(&integration.id).await.unwrap(),
        IntegrationAdapter::ComicVine(_)
    ));
}

#[tokio::test]
async fn omitted_secrets_preserve_and_null_clears_individually() {
    let directory = temporary_directory();
    let (_, settings) = service(directory.path()).await;
    let original = settings.create(input("Primary")).await.unwrap();
    let updated = settings
        .update(
            &original.id,
            serde_json::from_value(json!({"label":"Renamed"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.id, original.id);
    let private = settings.load_private(&original.id).await.unwrap();
    assert_eq!(private.api_key.as_deref(), Some("test-secret-api-key"));
    assert_eq!(private.password.as_deref(), Some("test-secret-password"));
    let updated = settings
        .update(
            &original.id,
            serde_json::from_value(json!({"api_key":null,"password":"replacement"})).unwrap(),
        )
        .await
        .unwrap();
    assert!(!updated.api_key_configured);
    assert!(!updated.credentials_configured);
    let private = settings.load_private(&original.id).await.unwrap();
    assert!(private.api_key.is_none());
    assert_eq!(private.username.as_deref(), Some("test-secret-username"));
    assert_eq!(private.password.as_deref(), Some("replacement"));
    assert!(matches!(
        settings.adapter(&original.id).await,
        Err(SettingsError::NotConfigured)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_key_creation_and_database_writes_do_not_deadlock() {
    let directory = temporary_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
        let directory = directory.path().to_owned();
        let store = store.clone();
        tasks.spawn(async move {
            let key = EncryptionKey::load_or_create(&directory).await.unwrap();
            let settings = Settings::new(store, key);
            settings
                .create(input(&format!("Concurrent {index}")))
                .await
                .unwrap();
        });
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("key initialization or DB write deadlocked");
    let key = EncryptionKey::load_or_create(directory.path())
        .await
        .unwrap();
    let settings = Settings::new(store, key);
    assert_eq!(settings.list().await.unwrap().len(), 16);
    for view in settings.list().await.unwrap() {
        assert_eq!(
            settings
                .load_private(&view.id)
                .await
                .unwrap()
                .api_key
                .as_deref(),
            Some("test-secret-api-key")
        );
    }
    assert!(
        !std::fs::read_dir(directory.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
}

#[tokio::test]
async fn concurrent_patches_preserve_unrelated_credentials() {
    let directory = temporary_directory();
    let (_, settings) = service(directory.path()).await;
    let integration = settings.create(input("Concurrent patch")).await.unwrap();
    let (first, second) = tokio::join!(
        settings.update(
            &integration.id,
            serde_json::from_value(json!({"api_key":"new-api-key"})).unwrap()
        ),
        settings.update(
            &integration.id,
            serde_json::from_value(json!({"password":"new-password"})).unwrap()
        )
    );
    first.unwrap();
    second.unwrap();
    let private = settings.load_private(&integration.id).await.unwrap();
    assert_eq!(private.api_key.as_deref(), Some("new-api-key"));
    assert_eq!(private.password.as_deref(), Some("new-password"));
}

#[tokio::test]
async fn rejects_tampering_nonce_version_wrong_key_and_row_substitution() {
    let directory = temporary_directory();
    let key = EncryptionKey::load_or_create(directory.path())
        .await
        .unwrap();
    let encrypted = key.encrypt("id-one", "comicvine", b"secret").unwrap();
    assert!(key.decrypt("id-two", "comicvine", &encrypted).is_err());
    assert!(key.decrypt("id-one", "prowlarr", &encrypted).is_err());
    let other = temporary_directory();
    let other = EncryptionKey::load_or_create(other.path()).await.unwrap();
    assert!(other.decrypt("id-one", "comicvine", &encrypted).is_err());
    let mut tampered = encrypted;
    tampered.version = 2;
    assert!(key.decrypt("id-one", "comicvine", &tampered).is_err());
    tampered.version = 1;
    tampered.nonce[0] ^= 1;
    assert!(key.decrypt("id-one", "comicvine", &tampered).is_err());
    tampered.nonce[0] ^= 1;
    tampered.ciphertext[0] ^= 1;
    assert!(key.decrypt("id-one", "comicvine", &tampered).is_err());
    let (store, settings) = service(directory.path()).await;
    let first = settings.create(input("First")).await.unwrap();
    let second = settings.create(input("Second")).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE integrations SET secret_nonce = (SELECT secret_nonce FROM integrations WHERE id = ?), secret_ciphertext = (SELECT secret_ciphertext FROM integrations WHERE id = ?) WHERE id = ?")
        .bind(&first.id).bind(&first.id).bind(&second.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        settings.load_private(&second.id).await,
        Err(SettingsError::Encryption)
    ));
    assert!(matches!(
        settings
            .update(&second.id, UpdateIntegration::default())
            .await,
        Err(SettingsError::Encryption)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn key_is_restrictive_and_rejects_invalid_files_without_replacement() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = temporary_directory();
    EncryptionKey::load_or_create(directory.path())
        .await
        .unwrap();
    let path = directory.path().join("encryption.key");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 32);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let original = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        EncryptionKey::load_or_create(directory.path())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"short").unwrap();
    assert!(
        EncryptionKey::load_or_create(directory.path())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"short");
    let linked = temporary_directory();
    symlink(&path, linked.path().join("encryption.key")).unwrap();
    assert!(EncryptionKey::load_or_create(linked.path()).await.is_err());
    let insecure = temporary_directory();
    std::fs::set_permissions(insecure.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        EncryptionKey::load_or_create(insecure.path())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::metadata(insecure.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(!insecure.path().join("encryption.key").exists());
}

#[tokio::test]
async fn constructs_every_supported_adapter_from_saved_settings() {
    let directory = temporary_directory();
    let (_, settings) = service(directory.path()).await;
    for (kind, options) in [
        ("comicvine", json!({})),
        ("mangaupdates", json!({})),
        ("mangadex", json!({})),
        ("getcomics", json!({})),
        (
            "prowlarr",
            json!({"indexer_id":7,"protocol":"torrent","categories":{"comics":[7030],"manga":[],"magazines":[7010]}}),
        ),
        (
            "sabnzbd",
            json!({"category":"comics","remote_path":"/downloads","local_path":"/mnt/downloads"}),
        ),
        ("qbittorrent", json!({"category":"comics"})),
    ] {
        let integration = settings.create(serde_json::from_value(json!({"kind":kind,"label":kind,"base_url":"http://127.0.0.1:12345/", "enabled":true, "options":options,"api_key":"key","username":"user","password":"pass"})).unwrap()).await.unwrap();
        assert!(settings.adapter(&integration.id).await.is_ok(), "{kind}");
    }
}

#[test]
fn prowlarr_options_are_typed_and_bounded() {
    let valid = json!({"indexer_id":7,"protocol":"usenet","categories":{"comics":[7030],"manga":[],"magazines":[7010]}});
    assert!(IntegrationOptions::parse(IntegrationKind::Prowlarr, valid.clone()).is_ok());
    for (field, value) in [
        ("indexer_id", json!(0)),
        ("indexer_id", json!(u32::MAX)),
        ("protocol", json!("ddl")),
        ("categories", json!({"comics":[0]})),
        ("categories", json!({"comics":vec![7030;101]})),
        ("categories", json!({"comics":[7030],"command":"execute"})),
    ] {
        let mut options = valid.clone();
        options[field] = value;
        assert!(IntegrationOptions::parse(IntegrationKind::Prowlarr, options).is_err());
    }
    assert!(
        IntegrationOptions::parse(IntegrationKind::MangaDex, json!({"api_key":"plaintext"}))
            .is_err()
    );
}

#[tokio::test]
async fn missing_key_with_encrypted_rows_fails_without_generating_a_replacement() {
    let directory = temporary_directory();
    let (store, settings) = service(directory.path()).await;
    let integration = settings.create(input("Persisted")).await.unwrap();
    let path = directory.path().join("encryption.key");
    let original = std::fs::read(&path).unwrap();
    drop(settings);
    store.close().await;
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        EncryptionKey::load_or_create(directory.path()).await,
        Err(SettingsError::MissingKey)
    ));
    assert!(!path.exists());
    // Restoring the original backup, rather than replacing the key, recovers the credentials.
    std::fs::write(&path, original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (_, settings) = service(directory.path()).await;
    assert_eq!(
        settings
            .load_private(&integration.id)
            .await
            .unwrap()
            .api_key
            .as_deref(),
        Some("test-secret-api-key")
    );
}

fn temporary_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    directory
}
