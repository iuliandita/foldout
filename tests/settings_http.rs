use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use libraryd::{
    auth::{AuthService, Scope},
    httpapi::{
        auth::AuthContext,
        settings::{SettingsContext, routes},
    },
    settings::{EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE: &str = "/api/v1/settings/integrations";
const ORIGIN: &str = "http://127.0.0.1:8787";

async fn app(directory: &std::path::Path) -> (Router, String, AuthService, SqliteStore) {
    let key = EncryptionKey::load_or_create(directory).await.unwrap();
    let store = SqliteStore::open(directory).await.unwrap();
    let service = AuthService::new(store.clone());
    let login = service
        .setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    let auth = AuthContext {
        service: service.clone(),
        origin: ORIGIN.into(),
    };
    let router = libraryd::httpapi::auth::routes(auth.clone()).merge(routes(SettingsContext {
        settings: Settings::new(store.clone(), key),
        auth,
    }));
    (
        router,
        format!("library_session={}", login.token),
        service,
        store,
    )
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    auth: Option<(&str, &str)>,
    origin: bool,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((name, value)) = auth {
        request = request.header(name, value);
    }
    if origin {
        request = request.header(header::ORIGIN, ORIGIN);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status.is_success() && status != StatusCode::NO_CONTENT {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn input() -> Value {
    json!({"kind":"qbittorrent","label":"Downloads","base_url":"http://127.0.0.1:8080/", "enabled":true,
        "options":{"category":"comics"}, "api_key":"secret-api-key-123", "username":"secret-user-456", "password":"secret-password-789"})
}

fn safe(value: &Value) {
    let text = value.to_string();
    for secret in [
        "secret-api-key-123",
        "secret-user-456",
        "secret-password-789",
        "secret_nonce",
        "secret_ciphertext",
        "\"api_key\":",
        "\"username\":",
        "\"password\":",
    ] {
        assert!(!text.contains(secret), "unsafe response");
    }
}

#[tokio::test]
async fn safe_crud_preserves_credentials_and_deletes_by_stable_id() {
    let directory = temporary_directory();
    let (app, cookie, _, _) = app(directory.path()).await;
    let auth = Some(("cookie", cookie.as_str()));
    let (status, created) = request(&app, "POST", BASE, auth, true, input()).await;
    assert_eq!(status, StatusCode::CREATED);
    safe(&created);
    assert_eq!(created["credentials_configured"], true);
    let id = created["id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    let path = format!("{BASE}/{id}");
    let (status, updated) =
        request(&app, "PATCH", &path, auth, true, json!({"label":"Renamed"})).await;
    assert_eq!(status, StatusCode::OK);
    safe(&updated);
    assert_eq!(updated["id"], id);
    assert_eq!(updated["password_configured"], true);
    let (_, cleared) = request(&app, "PATCH", &path, auth, true, json!({"password":null})).await;
    assert_eq!(cleared["password_configured"], false);
    assert_eq!(cleared["username_configured"], true);
    assert_eq!(cleared["credentials_configured"], false);
    let (status, list) = request(&app, "GET", BASE, auth, false, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    safe(&list);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(&app, "DELETE", &path, auth, true, Value::Null)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&app, "PATCH", &path, auth, true, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "DELETE", &path, auth, true, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn every_route_requires_admin_and_cookie_writes_require_origin() {
    let directory = temporary_directory();
    let (app, cookie, service, _) = app(directory.path()).await;
    for scope in [Scope::Read, Scope::Manage] {
        let key = service.create_key("restricted", scope).await.unwrap();
        let bearer = format!("Bearer {}", key.secret);
        for (method, path, body) in [
            ("GET", BASE.to_string(), Value::Null),
            ("POST", BASE.to_string(), input()),
            ("PATCH", format!("{BASE}/missing"), json!({})),
            ("DELETE", format!("{BASE}/missing"), Value::Null),
            ("POST", format!("{BASE}/missing/test"), Value::Null),
        ] {
            assert_eq!(
                request(&app, method, &path, None, true, body.clone())
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                request(
                    &app,
                    method,
                    &path,
                    Some(("authorization", &bearer)),
                    true,
                    body
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
    }
    for (method, path, body) in [
        ("POST", BASE.to_string(), input()),
        ("PATCH", format!("{BASE}/missing"), json!({})),
        ("DELETE", format!("{BASE}/missing"), Value::Null),
        ("POST", format!("{BASE}/missing/test"), Value::Null),
    ] {
        assert_eq!(
            request(&app, method, &path, Some(("cookie", &cookie)), false, body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn rejects_credential_urls_unknown_options_invalid_kinds_and_commands() {
    let directory = temporary_directory();
    let (app, cookie, _, _) = app(directory.path()).await;
    let auth = Some(("cookie", cookie.as_str()));
    for (field, value) in [
        (
            "base_url",
            json!("http://user:secret-password-789@example.invalid/"),
        ),
        (
            "base_url",
            json!("https://example.invalid/?api_key=secret-api-key-123"),
        ),
        ("base_url", json!("file:///etc/passwd")),
        ("base_url", json!("https://example.invalid/#token")),
        ("kind", json!("shell")),
        ("label", json!("")),
        ("password", json!("")),
        ("api_key", json!(42)),
        (
            "options",
            json!({"category":"comics","command":"touch /tmp/file"}),
        ),
        (
            "options",
            json!({"category":"comics","remote_path":"/downloads"}),
        ),
        (
            "options",
            json!({"category":"comics","remote_path":"/downloads","local_path":"../escape"}),
        ),
        (
            "options",
            json!({"category":"comics","remote_path":"/downloads","local_path":"/mnt/../escape"}),
        ),
    ] {
        let mut body = input();
        body[field] = value;
        let (status, error) = request(&app, "POST", BASE, auth, true, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "field {field}");
        safe(&error);
        assert!(error["error"]["trace_id"].is_string());
    }
    let (_, created) = request(&app, "POST", BASE, auth, true, input()).await;
    let path = format!("{BASE}/{}", created["id"].as_str().unwrap());
    for field in ["label", "base_url", "enabled", "options"] {
        let mut patch = json!({});
        patch[field] = Value::Null;
        assert_eq!(
            request(&app, "PATCH", &path, auth, true, patch).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&app, "PATCH", &path, auth, true, json!({"kind":"sabnzbd"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn tampered_credentials_return_generic_errors_without_ciphertext() {
    let directory = temporary_directory();
    let (app, cookie, _, store) = app(directory.path()).await;
    let auth = Some(("cookie", cookie.as_str()));
    let (_, created) = request(&app, "POST", BASE, auth, true, input()).await;
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE integrations SET secret_ciphertext = zeroblob(length(secret_ciphertext))")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for (method, path, body) in [
        ("GET", BASE.to_string(), Value::Null),
        (
            "PATCH",
            format!("{BASE}/{}", created["id"].as_str().unwrap()),
            json!({"password":"replacement"}),
        ),
    ] {
        let (status, error) = request(&app, method, &path, auth, true, body).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error["error"]["code"], "internal_error");
        safe(&error);
    }
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

#[tokio::test]
async fn connection_check_only_reads_client_status_and_keeps_credentials_private() {
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let mock = Router::new().route(
        "/api",
        axum::routing::post(
            move |axum::Form(fields): axum::Form<std::collections::HashMap<String, String>>| {
                let captured = captured.clone();
                async move {
                    assert_eq!(
                        fields.get("apikey").map(String::as_str),
                        Some("secret-api-key-123")
                    );
                    let mode = fields.get("mode").unwrap().clone();
                    captured.lock().unwrap().push(mode.clone());
                    axum::Json(match mode.as_str() {
                        "version" => json!({"version":"5.1.2"}),
                        "queue" => json!({"queue":{"slots":[]}}),
                        "history" => json!({"history":{"slots":[]}}),
                        _ => panic!("connection check attempted a mutation"),
                    })
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let directory = temporary_directory();
    let (app, cookie, _, _) = app(directory.path()).await;
    let auth = Some(("cookie", cookie.as_str()));
    let (status, created) = request(&app,"POST",BASE,auth,true,json!({"kind":"sabnzbd","label":"Test","base_url":format!("http://{address}/"),"enabled":true,"options":{"category":"test"},"api_key":"secret-api-key-123"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, result) = request(
        &app,
        "POST",
        &format!("{BASE}/{}/test", created["id"].as_str().unwrap()),
        auth,
        true,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    safe(&result);
    assert_eq!(result["version"], "5.1.2");
    assert_eq!(*requests.lock().unwrap(), ["version", "queue", "history"]);
    server.abort();
    let _ = server.await;
}
