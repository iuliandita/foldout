use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::{auth::AuthService, httpapi::errors::ApiError, store::sqlite::SqliteStore};

#[derive(Clone)]
struct Gate {
    auth: AuthService,
    token: Option<Arc<str>>,
    directory: PathBuf,
}

pub async fn protect(
    router: Router,
    store: SqliteStore,
    directory: &Path,
    require_token: bool,
) -> Result<Router, Box<dyn std::error::Error>> {
    let auth = AuthService::new(store);
    let configured = auth.configured().await?;
    if configured {
        remove_token(directory)
            .map_err(|_| std::io::Error::other("cannot durably remove the consumed setup token"))?;
    }
    let token = if require_token && !configured {
        Some(load_token(directory)?.into())
    } else {
        None
    };
    Ok(router.layer(middleware::from_fn_with_state(
        Gate {
            auth,
            token,
            directory: directory.to_owned(),
        },
        gate,
    )))
}

fn remove_token(directory: &Path) -> std::io::Result<()> {
    // Unlink the directory entry itself, including a symlink; never open its target.
    match std::fs::remove_file(directory.join("setup-token")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?
        .sync_all()
}

fn load_token(directory: &Path) -> std::io::Result<String> {
    let path = directory.join("setup-token");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            let mut bytes = [0_u8; 32];
            getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
            let token = URL_SAFE_NO_PAD.encode(bytes);
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            return Ok(token);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() != 43
    {
        return Err(std::io::Error::other(
            "setup-token must be a private service-owned token file",
        ));
    }
    let mut token = String::new();
    file.read_to_string(&mut token)?;
    if URL_SAFE_NO_PAD
        .decode(&token)
        .map_or(true, |bytes| bytes.len() != 32)
    {
        return Err(std::io::Error::other("setup-token is invalid"));
    }
    Ok(token)
}

async fn gate(State(state): State<Gate>, request: Request, next: Next) -> Response {
    if request.uri().path() != "/api/v1/auth/setup" {
        return next.run(request).await;
    }
    let configured = match state.auth.configured().await {
        Ok(configured) => configured,
        Err(_) => return ApiError::internal().into_response(),
    };
    if request.method() == Method::GET {
        return ([("cache-control", "no-store")], Json(serde_json::json!({"configured":configured, "requires_token":!configured && state.token.is_some()}))).into_response();
    }
    if request.method() == Method::POST
        && !configured
        && let Some(expected) = &state.token
    {
        let supplied = request
            .headers()
            .get("x-setup-token")
            .map(|value| value.as_bytes())
            .unwrap_or_default();
        let valid = supplied.len() == expected.len()
            && supplied
                .iter()
                .zip(expected.bytes())
                .fold(0_u8, |difference, (left, right)| {
                    difference | (left ^ right)
                })
                == 0;
        if !valid {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "setup_token_required",
                "Enter the setup token from the server state directory",
            )
            .into_response();
        }
    }
    let setup = request.method() == Method::POST;
    let response = next.run(request).await;
    if setup
        && response.status() == StatusCode::CREATED
        && let Err(error) = remove_token(&state.directory)
    {
        tracing::error!(kind = ?error.kind(), "setup committed but consumed token cleanup failed; configured startup will retry");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tower::ServiceExt;

    #[test]
    fn setup_token_is_private_valid_and_reused() {
        let directory = tempfile::tempdir().unwrap();
        let token = load_token(directory.path()).unwrap();
        let metadata = std::fs::metadata(directory.path().join("setup-token")).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 32);
        assert_eq!(load_token(directory.path()).unwrap(), token);
    }

    #[test]
    fn setup_token_rejects_existing_and_dangling_symlinks() {
        for existing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let target = directory.path().join("target");
            let token = URL_SAFE_NO_PAD.encode([1_u8; 32]);
            if existing {
                std::fs::write(&target, &token).unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let path = directory.path().join("setup-token");
            symlink(&target, &path).unwrap();
            assert!(load_token(directory.path()).is_err());
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            if existing {
                assert_eq!(std::fs::read_to_string(target).unwrap(), token);
            } else {
                assert!(!target.exists());
            }
        }
    }

    #[test]
    fn setup_token_rejects_wrong_modes_without_changing_the_file() {
        for mode in [0o400, 0o640, 0o644, 0o660, 0o700] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("setup-token");
            let token = URL_SAFE_NO_PAD.encode([1_u8; 32]);
            std::fs::write(&path, &token).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(load_token(directory.path()).is_err(), "mode {mode:o}");
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, mode);
            assert_eq!(std::fs::read_to_string(path).unwrap(), token);
        }
    }

    #[test]
    fn setup_token_rejects_truncated_invalid_and_padded_content() {
        let valid = URL_SAFE_NO_PAD.encode([1_u8; 32]);
        for token in [
            String::new(),
            valid[..42].to_owned(),
            "!".repeat(43),
            format!("{valid}="),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("setup-token");
            std::fs::write(&path, &token).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(load_token(directory.path()).is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), token);
        }
    }

    async fn setup_status(router: &Router) -> serde_json::Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn concurrent_invalid_setup_cannot_win_against_valid_token() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let router = protect(
            crate::app::router(store.clone()),
            store.clone(),
            directory.path(),
            true,
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(directory.path().join("setup-token")).unwrap();
        assert_eq!(
            setup_status(&router).await,
            serde_json::json!({"configured":false,"requires_token":true})
        );
        let setup_request = |username: &str, token: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/setup")
                .header("content-type", "application/json")
                .header("x-setup-token", token)
                .body(Body::from(
                    serde_json::json!({"username":username,"password":"private test password"})
                        .to_string(),
                ))
                .unwrap()
        };
        let wrong = if token.starts_with('A') {
            "B".repeat(43)
        } else {
            "A".repeat(43)
        };
        let (valid, invalid) = tokio::join!(
            router.clone().oneshot(setup_request("owner", &token)),
            router.clone().oneshot(setup_request("intruder", &wrong)),
        );
        assert_eq!(valid.unwrap().status(), StatusCode::CREATED);
        assert!(!directory.path().join("setup-token").exists());
        // If setup committed before the invalid request's check, setup returns conflict.
        assert!(matches!(
            invalid.unwrap().status(),
            StatusCode::FORBIDDEN | StatusCode::CONFLICT
        ));
        let users: Vec<String> = sqlx::query_scalar("SELECT username FROM auth_users")
            .fetch_all(store.reader())
            .await
            .unwrap();
        assert_eq!(users, vec!["owner".to_owned()]);
        assert_eq!(
            setup_status(&router).await,
            serde_json::json!({"configured":true,"requires_token":false})
        );
    }

    #[tokio::test]
    async fn remote_setup_requires_private_token_and_is_single_use() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("state");
        let store = SqliteStore::open(&directory).await.unwrap();
        let router = protect(
            crate::app::router(store.clone()),
            store.clone(),
            &directory,
            true,
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
        for (provided, expected) in [
            ("", StatusCode::FORBIDDEN),
            ("wrong", StatusCode::FORBIDDEN),
            (token.as_str(), StatusCode::CREATED),
            (token.as_str(), StatusCode::CONFLICT),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/auth/setup")
                        .header("content-type", "application/json")
                        .header("x-setup-token", provided)
                        .body(Body::from(
                            r#"{"username":"owner","password":"private test password"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if matches!(expected, StatusCode::CREATED | StatusCode::CONFLICT) {
                assert!(!directory.join("setup-token").exists());
            } else {
                assert!(directory.join("setup-token").exists());
            }
        }
        drop(router);
        store.close().await;
        std::fs::rename(&directory, parent.path().join("configured-state")).unwrap();
        let fresh_store = SqliteStore::open(&directory).await.unwrap();
        let fresh = protect(
            crate::app::router(fresh_store.clone()),
            fresh_store,
            &directory,
            true,
        )
        .await
        .unwrap();
        assert_ne!(
            std::fs::read_to_string(directory.join("setup-token")).unwrap(),
            token
        );
        assert_eq!(
            setup_status(&fresh).await,
            serde_json::json!({"configured":false,"requires_token":true})
        );
    }

    #[test]
    fn token_cleanup_unlinks_symlinks_without_touching_the_target_and_allows_absence() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, "preserve target").unwrap();
        let token = directory.path().join("setup-token");
        symlink(&target, &token).unwrap();
        remove_token(directory.path()).unwrap();
        assert!(std::fs::symlink_metadata(&token).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve target");
        remove_token(directory.path()).unwrap();
        symlink(directory.path().join("missing"), &token).unwrap();
        remove_token(directory.path()).unwrap();
        assert!(std::fs::symlink_metadata(token).is_err());
    }

    #[tokio::test]
    async fn configured_startup_removes_retained_token_and_retries_safely() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("state");
        let store = SqliteStore::open(&directory).await.unwrap();
        AuthService::new(store.clone())
            .setup("owner", "private test password")
            .await
            .unwrap();
        load_token(&directory).unwrap();
        for _ in 0..2 {
            let router = protect(
                crate::app::router(store.clone()),
                store.clone(),
                &directory,
                true,
            )
            .await
            .unwrap();
            assert!(!directory.join("setup-token").exists());
            assert_eq!(
                setup_status(&router).await,
                serde_json::json!({"configured":true,"requires_token":false})
            );
        }
        std::fs::create_dir(directory.join("setup-token")).unwrap();
        assert!(
            protect(crate::app::router(store.clone()), store, &directory, true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cleanup_failure_does_not_report_committed_setup_as_failed() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("state");
        let store = SqliteStore::open(&directory).await.unwrap();
        let router = protect(
            crate::app::router(store.clone()),
            store.clone(),
            &directory,
            true,
        )
        .await
        .unwrap();
        let path = directory.join("setup-token");
        let token = std::fs::read_to_string(&path).unwrap();
        std::fs::rename(&path, parent.path().join("saved-token")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/setup")
                    .header("content-type", "application/json")
                    .header("x-setup-token", token)
                    .body(Body::from(
                        r#"{"username":"owner","password":"private test password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(AuthService::new(store).configured().await.unwrap());
        assert!(path.is_dir());
    }
}
