use std::time::Duration;

use sqlx::Row;

use super::{AuthError, AuthService, Scope};
use crate::store::sqlite::SqliteStore;

#[tokio::test]
async fn validation_and_account_login_limits_are_isolated() {
    let directory = private_directory();
    let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
    for (username, password) in [("", "long enough password"), ("admin", "short")] {
        assert!(matches!(
            service.setup(username, password).await,
            Err(AuthError::Invalid(_))
        ));
    }
    assert!(matches!(
        service
            .setup(&"u".repeat(129), "long enough password")
            .await,
        Err(AuthError::Invalid(_))
    ));
    assert!(matches!(
        service.setup("admin", &"p".repeat(1025)).await,
        Err(AuthError::Invalid(_))
    ));
    assert!(!service.configured().await.unwrap());
    service
        .setup("admin", "long enough password")
        .await
        .unwrap();
    for _ in 0..11 {
        for (username, password) in [
            ("", "long enough password"),
            ("admin", "short"),
            (&"u".repeat(129), "long enough password"),
            ("admin", &"p".repeat(1025)),
        ] {
            assert!(matches!(
                service.login(username, password).await,
                Err(AuthError::Invalid(_))
            ));
        }
    }
    for _ in 0..10 {
        assert!(matches!(
            service.clone().login("unknown", "incorrect password").await,
            Err(AuthError::Unauthorized)
        ));
    }
    assert!(matches!(
        service.login("unknown", "incorrect password").await,
        Err(AuthError::RateLimited)
    ));
    service
        .login("admin", "long enough password")
        .await
        .unwrap();
    for _ in 0..9 {
        assert!(matches!(
            service.clone().login("admin", "incorrect password").await,
            Err(AuthError::Unauthorized)
        ));
    }
    assert!(matches!(
        service.login("admin", "long enough password").await,
        Err(AuthError::RateLimited)
    ));
}

#[tokio::test]
async fn peer_login_limits_validate_first_and_are_shared_only_with_the_same_ip() {
    let directory = private_directory();
    let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
    service
        .setup("admin", "long enough password")
        .await
        .unwrap();
    let first = "192.0.2.1".parse().unwrap();
    let second = "192.0.2.2".parse().unwrap();
    for _ in 0..11 {
        for (username, password) in [("", "long enough password"), ("admin", "short")] {
            assert!(matches!(
                service.login_from_peer(username, password, first).await,
                Err(AuthError::Invalid(_))
            ));
        }
    }
    for attempt in 0..10 {
        let username = if attempt % 2 == 0 { "admin" } else { "unknown" };
        assert!(matches!(
            service
                .clone()
                .login_from_peer(username, "incorrect password", first)
                .await,
            Err(AuthError::Unauthorized)
        ));
    }
    assert!(matches!(
        service
            .login_from_peer("admin", "long enough password", first)
            .await,
        Err(AuthError::RateLimited)
    ));
    service
        .login_from_peer("admin", "long enough password", second)
        .await
        .unwrap();
    service
        .login("admin", "long enough password")
        .await
        .unwrap();
}

#[tokio::test]
async fn keys_list_without_secrets_and_logout_revokes_only_its_session() {
    let directory = private_directory();
    let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
    let timestamp = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    let before_setup = timestamp();
    let first = service
        .setup("admin", "long enough password")
        .await
        .unwrap();
    let after_setup = timestamp();
    assert!((before_setup + 86_400..=after_setup + 86_400).contains(&first.expires_at));
    let second = service
        .login("admin", "long enough password")
        .await
        .unwrap();
    assert_eq!(first.token.len(), "session_".len() + 43);
    for scope in [Scope::Read, Scope::Manage, Scope::Admin] {
        let key = service.create_key("automation", scope).await.unwrap();
        assert_eq!(key.secret.len(), "key_".len() + 43);
        let principal = service.authenticate(&key.secret).await.unwrap();
        assert_eq!(principal.kind, super::CredentialKind::ApiKey);
        assert_eq!(principal.user_id, first.principal.user_id);
        assert_eq!(principal.scope, scope);
        let list = serde_json::to_value(service.list_keys().await.unwrap()).unwrap();
        for info in list.as_array().unwrap() {
            assert!(info.get("secret").is_none());
            assert!(info.get("token_digest").is_none());
        }
        assert!(!list.to_string().contains(&key.secret));
    }
    service.logout(&first.token).await.unwrap();
    assert!(matches!(
        service.authenticate(&first.token).await,
        Err(AuthError::Unauthorized)
    ));
    assert_eq!(
        service.authenticate(&second.token).await.unwrap().kind,
        super::CredentialKind::Session
    );
    assert!(Scope::Admin.allows(Scope::Manage));
    assert!(Scope::Manage.allows(Scope::Read));
    assert!(!Scope::Manage.allows(Scope::Admin));
}

#[tokio::test]
async fn same_username_setup_race_returns_conflict() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let first = AuthService::new(store.clone());
    let second = AuthService::new(store);
    let (first, second) = tokio::join!(
        first.setup("admin", "correct horse battery staple"),
        second.setup("admin", "correct horse battery staple"),
    );
    assert!(first.is_ok() ^ second.is_ok());
    assert!(
        matches!(first, Err(AuthError::AlreadyConfigured))
            || matches!(second, Err(AuthError::AlreadyConfigured))
    );
}

#[tokio::test]
async fn setup_has_one_race_winner() {
    let directory = private_directory();
    let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
    let first = service.clone();
    let second = service.clone();

    let (first, second) = tokio::join!(
        first.setup("admin", "correct horse battery staple"),
        second.setup("other", "correct horse battery staple"),
    );

    assert!(first.is_ok() ^ second.is_ok());
    assert!(
        matches!(first, Err(AuthError::AlreadyConfigured))
            || matches!(second, Err(AuthError::AlreadyConfigured))
    );
}

#[tokio::test]
async fn password_hashes_and_secret_digests_are_not_plaintext() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let service = AuthService::new(store.clone());
    let password = "correct horse battery staple";
    let login = service.setup("admin", password).await.unwrap();
    let key = service.create_key("reader", Scope::Read).await.unwrap();

    let user: String = sqlx::query("SELECT password_hash FROM auth_users")
        .fetch_one(store.reader())
        .await
        .unwrap()
        .get(0);
    let session: String = sqlx::query("SELECT token_digest FROM auth_sessions")
        .fetch_one(store.reader())
        .await
        .unwrap()
        .get(0);
    let api_key: String = sqlx::query("SELECT token_digest FROM auth_keys")
        .fetch_one(store.reader())
        .await
        .unwrap()
        .get(0);

    assert!(user.starts_with("$argon2id$"));
    assert_ne!(user, password);
    assert_ne!(session, login.token);
    assert_ne!(api_key, key.secret);
}

#[tokio::test]
async fn login_key_scopes_revocation_and_session_expiry_work() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let service = AuthService::new(store.clone());
    let login = service
        .setup("admin", "correct horse battery staple")
        .await
        .unwrap();
    assert_eq!(
        service
            .login("admin", "correct horse battery staple")
            .await
            .unwrap()
            .principal
            .scope,
        Scope::Admin
    );

    let key = service.create_key("reader", Scope::Read).await.unwrap();
    let principal = service.authenticate(&key.secret).await.unwrap();
    assert_eq!(principal.scope, Scope::Read);
    assert!(!principal.scope.allows(Scope::Manage));
    service.revoke_key(&key.id).await.unwrap();
    assert!(matches!(
        service.authenticate(&key.secret).await,
        Err(AuthError::Unauthorized)
    ));

    let mut transaction = store.begin_write().await.unwrap();
    sqlx::query("UPDATE auth_sessions SET expires_at = 0")
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert!(matches!(
        service.authenticate(&login.token).await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn auth_survives_reopen() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let service = AuthService::new(store.clone());
    let login = service
        .setup("admin", "correct horse battery staple")
        .await
        .unwrap();
    store.close().await;

    let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
    assert!(service.configured().await.unwrap());
    assert_eq!(
        service.authenticate(&login.token).await.unwrap().scope,
        Scope::Admin
    );
    assert_eq!(
        service
            .login("admin", "correct horse battery staple")
            .await
            .unwrap()
            .principal
            .scope,
        Scope::Admin
    );
}

#[tokio::test]
async fn settings_writes_complete_while_password_hashing_runs() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let service = AuthService::new(store.clone());
    let setup =
        tokio::spawn(async move { service.setup("admin", "correct horse battery staple").await });

    let writes = async {
        for value in 0..20 {
            let mut transaction = store.begin_write().await.unwrap();
            sqlx::query("INSERT INTO service_settings (name, value) VALUES ('auth-hash-write', ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value")
                .bind(value.to_string())
                .execute(&mut *transaction)
                .await
                .unwrap();
            transaction.commit().await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), writes)
        .await
        .unwrap();
    setup.await.unwrap().unwrap();
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
