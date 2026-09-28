use super::{AuthError, CreatedKey, CredentialKind, KeyInfo, Login, Principal, Scope};
use crate::store::sqlite::SqliteStore;
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_ATTEMPTS: usize = 10;
const LOGIN_BUCKETS: usize = 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
enum LoginBucket {
    Peer(IpAddr),
    Account(String),
}

#[derive(Clone)]
pub struct AuthService {
    store: SqliteStore,
    hashing: Arc<Semaphore>,
    attempts: Arc<Mutex<HashMap<LoginBucket, VecDeque<Instant>>>>,
}

impl AuthService {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            hashing: Arc::new(Semaphore::new(2)),
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn configured(&self) -> Result<bool, AuthError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM auth_users)")
                .fetch_one(self.store.reader())
                .await?
                != 0,
        )
    }

    pub async fn setup(&self, username: &str, password: &str) -> Result<Login, AuthError> {
        validate_credentials(username, password)?;
        if self.configured().await? {
            return Err(AuthError::AlreadyConfigured);
        }
        let password_hash = self.password_work(password, None).await?;
        let user_id = Uuid::new_v4().to_string();
        let login = new_session(user_id.clone())?;
        let mut transaction = self.store.begin_write().await?;
        let inserted = sqlx::query("INSERT INTO auth_users(id, username, password_hash) VALUES (?, ?, ?) ON CONFLICT DO NOTHING")
            .bind(&user_id).bind(username).bind(password_hash).execute(&mut *transaction).await?;
        if inserted.rows_affected() == 0 {
            return Err(AuthError::AlreadyConfigured);
        }
        save_session(&mut transaction, &login).await?;
        transaction.commit().await?;
        Ok(login)
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<Login, AuthError> {
        validate_credentials(username, password)?;
        self.limit_login(LoginBucket::Account(username.to_owned()))?;
        self.login_validated(username, password).await
    }

    /// The peer must come from the transport, never a client-supplied header.
    pub async fn login_from_peer(
        &self,
        username: &str,
        password: &str,
        peer: IpAddr,
    ) -> Result<Login, AuthError> {
        validate_credentials(username, password)?;
        self.limit_login(LoginBucket::Peer(peer))?;
        self.login_validated(username, password).await
    }

    async fn login_validated(&self, username: &str, password: &str) -> Result<Login, AuthError> {
        let user = sqlx::query("SELECT id, password_hash FROM auth_users WHERE username = ?")
            .bind(username)
            .fetch_optional(self.store.reader())
            .await?;
        let hash = user
            .as_ref()
            .map(|row| row.get::<String, _>("password_hash"))
            .unwrap_or_else(|| DUMMY_HASH.to_owned());
        self.password_work(password, Some(hash)).await?;
        let user = user.ok_or(AuthError::Unauthorized)?;
        let login = new_session(user.get("id"))?;
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("DELETE FROM auth_sessions WHERE expires_at <= ?")
            .bind(now()?)
            .execute(&mut *transaction)
            .await?;
        save_session(&mut transaction, &login).await?;
        transaction.commit().await?;
        Ok(login)
    }

    fn limit_login(&self, bucket: LoginBucket) -> Result<(), AuthError> {
        let mut buckets = self.attempts.lock().map_err(|_| AuthError::Unavailable)?;
        let now = Instant::now();
        buckets.retain(|_, attempts| {
            attempts
                .back()
                .is_some_and(|at| now.duration_since(*at) < LOGIN_WINDOW)
        });
        if !buckets.contains_key(&bucket) && buckets.len() >= LOGIN_BUCKETS {
            let oldest = buckets
                .iter()
                .min_by_key(|(_, attempts)| attempts.back().copied())
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                buckets.remove(&oldest);
            }
        }
        let attempts = buckets.entry(bucket).or_default();
        while attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= LOGIN_WINDOW)
        {
            attempts.pop_front();
        }
        if attempts.len() >= LOGIN_ATTEMPTS {
            return Err(AuthError::RateLimited);
        }
        attempts.push_back(now);
        Ok(())
    }

    async fn password_work(
        &self,
        password: &str,
        hash: Option<String>,
    ) -> Result<String, AuthError> {
        let permit = self
            .hashing
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::Unavailable)?;
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || {
            // The blocking job keeps its permit even if the request is canceled.
            let _permit = permit;
            let params =
                Params::new(19 * 1024, 2, 1, Some(32)).map_err(|_| AuthError::Unavailable)?;
            let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            if let Some(hash) = hash {
                argon
                    .verify_password(password.as_bytes(), hash.as_str())
                    .map_err(|_| AuthError::Unauthorized)?;
                Ok(String::new())
            } else {
                let mut salt = [0u8; 16];
                getrandom::fill(&mut salt).map_err(|_| AuthError::Unavailable)?;
                Ok(argon
                    .hash_password_with_salt(password.as_bytes(), &salt)
                    .map_err(|_| AuthError::Unavailable)?
                    .to_string())
            }
        })
        .await
        .map_err(|_| AuthError::Unavailable)?
    }

    pub async fn authenticate(&self, token: &str) -> Result<Principal, AuthError> {
        if token.len() > 256 {
            return Err(AuthError::Unauthorized);
        }
        if token.starts_with("session_") {
            let user_id = sqlx::query_scalar(
                "SELECT user_id FROM auth_sessions WHERE token_digest = ? AND expires_at > ?",
            )
            .bind(digest(token))
            .bind(now()?)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(AuthError::Unauthorized)?;
            Ok(Principal {
                user_id,
                scope: Scope::Admin,
                kind: CredentialKind::Session,
            })
        } else if token.starts_with("key_") {
            let row = sqlx::query("SELECT user_id, scope FROM auth_keys WHERE token_digest = ?")
                .bind(digest(token))
                .fetch_optional(self.store.reader())
                .await?
                .ok_or(AuthError::Unauthorized)?;
            Ok(Principal {
                user_id: row.get("user_id"),
                scope: parse_scope(row.get("scope"))?,
                kind: CredentialKind::ApiKey,
            })
        } else {
            Err(AuthError::Unauthorized)
        }
    }

    pub async fn logout(&self, token: &str) -> Result<(), AuthError> {
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("DELETE FROM auth_sessions WHERE token_digest = ?")
            .bind(digest(token))
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn create_key(&self, name: &str, scope: Scope) -> Result<CreatedKey, AuthError> {
        if name.trim().is_empty() || name.len() > 128 {
            return Err(AuthError::Invalid(
                "Key name must contain 1 to 128 bytes".into(),
            ));
        }
        let key = CreatedKey {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
            scope,
            secret: random_token("key_")?,
        };
        let mut transaction = self.store.begin_write().await?;
        let user_id: String = sqlx::query_scalar("SELECT id FROM auth_users")
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AuthError::Unauthorized)?;
        sqlx::query("INSERT INTO auth_keys(id, user_id, name, scope, token_digest, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&key.id).bind(user_id).bind(name).bind(scope.as_str()).bind(digest(&key.secret)).bind(now()?).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(key)
    }

    pub async fn list_keys(&self) -> Result<Vec<KeyInfo>, AuthError> {
        sqlx::query("SELECT id, name, scope, created_at FROM auth_keys ORDER BY created_at, id")
            .fetch_all(self.store.reader())
            .await?
            .into_iter()
            .map(|row| {
                Ok(KeyInfo {
                    id: row.get("id"),
                    name: row.get("name"),
                    scope: parse_scope(row.get("scope"))?,
                    created_at: row.get("created_at"),
                })
            })
            .collect()
    }

    pub async fn revoke_key(&self, id: &str) -> Result<(), AuthError> {
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("DELETE FROM auth_keys WHERE id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

fn validate_credentials(username: &str, password: &str) -> Result<(), AuthError> {
    if username.trim().is_empty() || username.len() > 128 {
        return Err(AuthError::Invalid(
            "Username must contain 1 to 128 bytes".into(),
        ));
    }
    if !(12..=1024).contains(&password.len()) {
        return Err(AuthError::Invalid(
            "Password must contain 12 to 1024 bytes".into(),
        ));
    }
    Ok(())
}

fn now() -> Result<i64, AuthError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::Unavailable)?
        .as_secs() as i64)
}

fn random_token(prefix: &str) -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| AuthError::Unavailable)?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn digest(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn parse_scope(scope: &str) -> Result<Scope, AuthError> {
    match scope {
        "read" => Ok(Scope::Read),
        "manage" => Ok(Scope::Manage),
        "admin" => Ok(Scope::Admin),
        _ => Err(AuthError::Unavailable),
    }
}

fn new_session(user_id: String) -> Result<Login, AuthError> {
    Ok(Login {
        token: random_token("session_")?,
        expires_at: now()? + 24 * 60 * 60,
        principal: Principal {
            user_id,
            scope: Scope::Admin,
            kind: CredentialKind::Session,
        },
    })
}

async fn save_session(
    transaction: &mut Transaction<'_, Sqlite>,
    login: &Login,
) -> Result<(), AuthError> {
    sqlx::query("INSERT INTO auth_sessions(token_digest, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(digest(&login.token))
        .bind(&login.principal.user_id)
        .bind(login.expires_at)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    fn private_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[tokio::test]
    async fn login_buckets_are_bounded_and_evict_expired_then_oldest() {
        let directory = private_directory();
        let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
        let now = Instant::now();
        let oldest = LoginBucket::Account("oldest".into());
        let expired = LoginBucket::Account("expired".into());
        {
            let mut buckets = service.attempts.lock().unwrap();
            buckets.insert(
                oldest.clone(),
                VecDeque::from([now - Duration::from_secs(30)]),
            );
            buckets.insert(expired.clone(), VecDeque::from([now - LOGIN_WINDOW]));
            for index in 0..LOGIN_BUCKETS - 2 {
                buckets.insert(
                    LoginBucket::Account(index.to_string()),
                    VecDeque::from([now]),
                );
            }
        }
        service
            .limit_login(LoginBucket::Account("new".into()))
            .unwrap();
        {
            let buckets = service.attempts.lock().unwrap();
            assert_eq!(buckets.len(), LOGIN_BUCKETS);
            assert!(!buckets.contains_key(&expired));
            assert!(buckets.contains_key(&oldest));
        }
        service
            .limit_login(LoginBucket::Account("newer".into()))
            .unwrap();
        let buckets = service.attempts.lock().unwrap();
        assert_eq!(buckets.len(), LOGIN_BUCKETS);
        assert!(!buckets.contains_key(&oldest));
    }

    #[tokio::test]
    async fn login_window_expires_attempts_without_resetting_recent_attempts() {
        let directory = private_directory();
        let service = AuthService::new(SqliteStore::open(directory.path()).await.unwrap());
        let bucket = LoginBucket::Peer("2001:db8::1".parse().unwrap());
        let now = Instant::now();
        let mut attempts = VecDeque::from(vec![now; LOGIN_ATTEMPTS]);
        attempts[0] = now - LOGIN_WINDOW;
        service
            .attempts
            .lock()
            .unwrap()
            .insert(bucket.clone(), attempts);
        service.clone().limit_login(bucket.clone()).unwrap();
        assert!(matches!(
            service.limit_login(bucket.clone()),
            Err(AuthError::RateLimited)
        ));
        assert_eq!(
            service.attempts.lock().unwrap()[&bucket].len(),
            LOGIN_ATTEMPTS
        );
        service.attempts.lock().unwrap().insert(
            bucket.clone(),
            VecDeque::from(vec![now - LOGIN_WINDOW; LOGIN_ATTEMPTS]),
        );
        service.limit_login(bucket.clone()).unwrap();
        assert_eq!(service.attempts.lock().unwrap()[&bucket].len(), 1);
    }
}
