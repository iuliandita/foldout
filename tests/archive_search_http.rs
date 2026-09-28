use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::get,
};
use http_body_util::BodyExt;
use libraryd::{
    auth::{AuthService, Scope},
    httpapi::{
        auth::AuthContext,
        search::{SearchContext, routes},
    },
    search::Search,
    settings::{EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn archive_search_requires_manage_and_preserves_issue_identity_and_cooldown() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(directory.path())
            .await
            .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let fixture = Router::new().route("/advancedsearch.php", get(|| async { Json(json!({"response":{"numFound":1,"start":0,"docs":[{"identifier":"owned-magazine","title":"Owned magazine January/February","date":"2026-01","language":["eng"],"collection":["magazine_rack"],"mediatype":"texts"}]}})) }));
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let integration = settings.create(serde_json::from_value(json!({"kind":"internetarchive","label":"Owned archive","base_url":base,"enabled":true})).unwrap()).await.unwrap();
    let alias = settings.create(serde_json::from_value(json!({"kind":"internetarchive","label":"Same archive","base_url":base.trim_end_matches('/'),"enabled":true})).unwrap()).await.unwrap();
    let auth = AuthService::new(store.clone());
    auth.setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    let read = auth.create_key("read", Scope::Read).await.unwrap();
    let manage = auth.create_key("manage", Scope::Manage).await.unwrap();
    let app = routes(SearchContext {
        search: Search::new(store.clone(), settings),
        auth: AuthContext {
            service: auth,
            origin: "http://localhost".into(),
        },
    });
    let path = format!(
        "/api/v1/search/archive?integration_id={}&query=Owned&page=1&limit=20",
        integration.id
    );
    for key in [None, Some(read.secret.as_str())] {
        let (status, _) = call(&app, &path, key).await;
        assert_eq!(
            status,
            if key.is_none() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            }
        );
    }
    let invalid_item = format!(
        "/api/v1/search/archive/item?integration_id={}&identifier=..",
        integration.id
    );
    assert_eq!(
        call(&app, &invalid_item, Some(&manage.secret)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM search_cooldowns")
            .fetch_one(store.reader())
            .await
            .unwrap(),
        0
    );
    let (status, value) = call(&app, &path, Some(&manage.secret)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["items"][0]["identifier"], "owned-magazine");
    assert_eq!(value["items"][0]["dates"], json!(["2026-01"]));
    assert_eq!(value["items"][0]["countries"], json!([]));
    assert_eq!(value["items"][0]["issues"], json!([]));
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE search_cooldowns SET next_at = unixepoch() + 60")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let alias_path = path.replace(&integration.id, &alias.id);
    assert_eq!(
        call(&app, &alias_path, Some(&manage.secret)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &path, Some(&manage.secret)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &format!("{path}&unexpected=1"), Some(&manage.secret))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let item = format!(
        "/api/v1/search/archive/item?integration_id={}&identifier=owned-magazine",
        integration.id
    );
    assert_eq!(
        call(&app, &item, Some(&read.secret)).await.0,
        StatusCode::FORBIDDEN
    );
    server.abort();
    let _ = server.await;
    store.close().await;
}

async fn call(app: &Router, path: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status.is_success() {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}
