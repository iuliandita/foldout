use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use libraryd::{app, store::sqlite::SqliteStore};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    body: Value,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = cookie {
        request = request.header(header::COOKIE, value);
    }
    if let Some(value) = origin {
        request = request.header(header::ORIGIN, value);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn setup_cookie_requires_origin_for_writes_and_logout_invalidates_it() {
    let directory = private_directory();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let app = app::router(store);
    let response = request(
        &app,
        "POST",
        "/api/v1/auth/setup",
        None,
        Some("http://127.0.0.1:8787"),
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let cookie = cookie.split(';').next().unwrap();
    let response = request(
        &app,
        "POST",
        "/api/v1/auth/keys",
        Some(cookie),
        None,
        json!({"name":"automation","scope":"read"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = request(
        &app,
        "DELETE",
        "/api/v1/auth/session",
        Some(cookie),
        Some("http://127.0.0.1:8787"),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = request(
        &app,
        "GET",
        "/api/v1/auth/session",
        Some(cookie),
        None,
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_json_and_unknown_api_return_structured_errors() {
    let directory = private_directory();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
    for (path, status) in [
        ("/api/v1/auth/setup", StatusCode::BAD_REQUEST),
        ("/api/v1/missing", StatusCode::NOT_FOUND),
    ] {
        let response = request(
            &app,
            "POST",
            path,
            None,
            None,
            json!({"unexpected":"field"}),
        )
        .await;
        assert_eq!(response.status(), status);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"]["trace_id"].is_string());
        assert!(!String::from_utf8_lossy(&body).contains("sqlite"));
    }
}

#[tokio::test]
async fn cross_origin_setup_is_forbidden() {
    let directory = private_directory();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
    let response = request(
        &app,
        "POST",
        "/api/v1/auth/setup",
        None,
        Some("https://untrusted.example"),
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

async fn login_request(
    app: &Router,
    peer: Option<SocketAddr>,
    forwarded: &str,
    body: Value,
) -> StatusCode {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-for", forwarded)
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn login_throttles_socket_ips_and_ignores_forwarded_headers() {
    let directory = private_directory();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
    let credentials = json!({"username":"owner","password":"correct horse battery staple"});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            None,
            None,
            credentials.clone()
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    let first: SocketAddr = "192.0.2.1:1000".parse().unwrap();
    for _ in 0..11 {
        for invalid in [
            json!({"username":"owner","password":"short"}),
            json!({"username":"","password":"correct horse battery staple"}),
            json!({"unexpected":"field"}),
        ] {
            assert_eq!(
                login_request(&app, Some(first), "198.51.100.1", invalid).await,
                StatusCode::BAD_REQUEST
            );
        }
    }
    for attempt in 0..10 {
        let peer = SocketAddr::new(first.ip(), 1000 + attempt);
        assert_eq!(
            login_request(
                &app,
                Some(peer),
                &format!("198.51.100.{attempt}"),
                json!({"username":format!("unknown-{attempt}"),"password":"incorrect password"})
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_request(&app, Some(first), "198.51.100.100", credentials.clone()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_request(
            &app,
            Some("192.0.2.2:1000".parse().unwrap()),
            "192.0.2.1",
            credentials
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn login_without_peer_uses_isolated_account_limits() {
    let directory = private_directory();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
    let credentials = json!({"username":"owner","password":"correct horse battery staple"});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/auth/setup",
            None,
            None,
            credentials.clone()
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    let unknown = json!({"username":"unknown","password":"incorrect password"});
    for attempt in 0..10 {
        assert_eq!(
            login_request(
                &app,
                None,
                &format!("198.51.100.{attempt}"),
                unknown.clone()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_request(&app, None, "198.51.100.100", unknown).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_request(&app, None, "198.51.100.100", credentials).await,
        StatusCode::OK
    );
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
