use crate::store::sqlite::SqliteStore;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;
use std::time::Duration;

pub fn router(store: SqliteStore) -> Router {
    router_with_origin(store, "http://127.0.0.1:8787")
}

pub fn router_with_origin(store: SqliteStore, origin: &str) -> Router {
    build_router(store, origin, None)
}

pub fn router_with_settings(
    store: SqliteStore,
    origin: &str,
    settings: crate::settings::Settings,
) -> Router {
    build_router(store, origin, Some(settings))
}

fn build_router(
    store: SqliteStore,
    origin: &str,
    settings: Option<crate::settings::Settings>,
) -> Router {
    let auth = crate::httpapi::auth::AuthContext {
        service: crate::auth::AuthService::new(store.clone()),
        origin: origin.into(),
    };
    let router = Router::new()
        .route(
            "/health/live",
            get(|| async { Json(Health { status: "alive" }) }),
        )
        .route("/health/ready", get(ready))
        .fallback(get(crate::webassets::serve))
        .with_state(store.clone())
        .merge(crate::httpapi::auth::routes(auth.clone()))
        .merge(crate::httpapi::selection::routes(
            crate::httpapi::selection::SelectionContext {
                repository: crate::search::selection::SelectionRepository::new(store.clone()),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::reader::routes(
            crate::httpapi::reader::ReaderContext {
                service: crate::reader::service::ReaderService::new(store.clone()),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::library::routes(
            crate::httpapi::library::LibraryContext {
                library: crate::library::roots::Library::new(store.clone()),
                previews: crate::importer::preview::PreviewService::new(store.clone()),
                jobs: crate::jobs::Jobs::new(store.clone()),
                store: store.clone(),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::review::routes(
            crate::httpapi::review::ReviewContext {
                store: store.clone(),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::jobs::routes(
            crate::httpapi::jobs::JobContext {
                jobs: crate::jobs::Jobs::new(store.clone()),
                library: crate::library::roots::Library::new(store.clone()),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::wanted::routes(
            crate::httpapi::wanted::WantedContext {
                repository: crate::catalog::wanted::WantedRepository::new(store.clone()),
                auth: auth.clone(),
            },
        ))
        .merge(crate::httpapi::catalog::routes(
            crate::httpapi::catalog::CatalogContext {
                repository: crate::catalog::CatalogRepository::new(store.clone()),
                auth: auth.clone(),
            },
        ))
        .route(
            "/api/{*path}",
            axum::routing::any(crate::httpapi::errors::not_found),
        );
    let router = if let Some(settings) = settings {
        router
            .merge(crate::httpapi::direct::routes(
                crate::httpapi::direct::DirectContext {
                    direct: crate::acquisition::direct::Direct::new(store.clone()),
                    settings: settings.clone(),
                    auth: auth.clone(),
                },
            ))
            .merge(crate::httpapi::monitor::routes(
                crate::httpapi::monitor::MonitorContext {
                    monitor: crate::acquisition::monitor::Monitor::new(
                        store.clone(),
                        settings.clone(),
                    ),
                    auth: auth.clone(),
                },
            ))
            .merge(crate::httpapi::search::routes(
                crate::httpapi::search::SearchContext {
                    search: crate::search::Search::new(store.clone(), settings.clone()),
                    auth: auth.clone(),
                },
            ))
            .merge(crate::httpapi::acquisition::routes(
                crate::httpapi::acquisition::AcquisitionContext {
                    pipeline: crate::acquisition::pipeline::Pipeline::new(store),
                    settings: settings.clone(),
                    auth: auth.clone(),
                },
            ))
            .merge(crate::httpapi::settings::routes(
                crate::httpapi::settings::SettingsContext { settings, auth },
            ))
    } else {
        router
    };
    router.layer(axum::middleware::from_fn(response_headers))
}

async fn response_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let private = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    use axum::http::{HeaderValue, header};
    // Handlers that set their own policy (thumbnails: private, validated by ETag) keep it.
    if private && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"));
    response
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn ready(State(store): State<SqliteStore>) -> (StatusCode, Json<Health>) {
    match tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(store.reader()),
    )
    .await
    {
        Ok(Ok(1)) => (StatusCode::OK, Json(Health { status: "ready" })),
        result => {
            match result {
                Ok(Err(error)) => tracing::warn!(%error, "database readiness check failed"),
                Err(_) => tracing::warn!("database readiness check exceeded 2 seconds"),
                Ok(Ok(_)) => {
                    tracing::warn!("database readiness check returned an unexpected result")
                }
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(Health {
                    status: "unavailable",
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn private_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[tokio::test]
    async fn readiness_fails_when_database_is_closed_but_liveness_survives() {
        let directory = private_directory();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let app = router(store.clone());
        store.close().await;
        for (path, status) in [
            ("/health/live", StatusCode::OK),
            ("/health/ready", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
    }

    #[tokio::test]
    async fn shell_has_explicit_build_state_and_missing_assets_are_not_html() {
        let directory = private_directory();
        let app = router(SqliteStore::open(directory.path()).await.unwrap());
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if cfg!(feature = "embedded-ui") {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assets/missing.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn health_reports_database_availability() {
        let directory = private_directory();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let app = router(store);
        for path in ["/health/live", "/health/ready"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
            assert_eq!(response.headers()["content-type"], "application/json");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                health["status"],
                if path.ends_with("live") {
                    "alive"
                } else {
                    "ready"
                }
            );
        }
    }

    #[tokio::test]
    async fn unknown_api_does_not_return_the_app_shell() {
        let directory = private_directory();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let response = router(store)
            .oneshot(
                Request::builder()
                    .uri("/api/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
