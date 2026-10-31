use axum::{
    Json, Router,
    body::Body,
    extract::Path,
    http::{Request, StatusCode, header},
    routing::get,
};
use http_body_util::BodyExt;
use libraryd::{
    auth::{AuthService, Scope},
    catalog::{CatalogRepository, NewEdition, NewPublication, NewUnit},
    httpapi::{
        auth::AuthContext,
        search::{SearchContext, routes},
    },
    search::Search,
    settings::{EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

struct Fixture {
    directory: tempfile::TempDir,
    store: SqliteStore,
    settings: Settings,
    auth: AuthContext,
    app: Router,
    read: String,
    manage: String,
    cookie: String,
    integration: String,
    edition: String,
    requests: Arc<AtomicUsize>,
    combined_label: Arc<Mutex<String>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(directory.path())
                .await
                .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let combined_label = Arc::new(Mutex::new("3-4".to_string()));
        let raw_label = combined_label.clone();
        let provider = Router::new().route("/metadata/{identifier}", get(move |Path(identifier): Path<String>| {
            let count = count.clone();
            let raw_label = raw_label.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                let (issue, date) = match identifier.as_str() {
                    "padded" => (" 9 ".to_string(), json!(" 2024-03 ")),
                    "year" => ("1".to_string(), json!("2024")),
                    "month" => ("2".to_string(), json!("2024-02")),
                    "day" => ("5".to_string(), json!("2024-02-29")),
                    "ambiguous" => ("6".to_string(), json!(["2024-01", "2024-02"])),
                    "text-combined" => ("Jan/Feb".to_string(), json!("January/February 2024")),
                    "combined" => (raw_label.lock().unwrap().clone(), json!("2025-01")),
                    _ => ("other".to_string(), json!(null)),
                };
                Json(json!({"metadata":{"identifier":identifier,"title":"Owned magazine","mediatype":"texts","collection":["magazine_rack"],"issue":issue,"date":date},"files":[]}))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let integration = settings.create(serde_json::from_value(json!({"kind":"internetarchive","label":"Owned fixture","base_url":base,"enabled":true})).unwrap()).await.unwrap().id;
        let service = AuthService::new(store.clone());
        let session = service
            .setup("owner", "correct horse battery staple")
            .await
            .unwrap();
        let read = service
            .create_key("read", Scope::Read)
            .await
            .unwrap()
            .secret;
        let manage = service
            .create_key("manage", Scope::Manage)
            .await
            .unwrap()
            .secret;
        let auth = AuthContext {
            service,
            origin: "http://localhost".into(),
        };
        let app = routes(SearchContext {
            search: Search::new(store.clone(), settings.clone()),
            auth: auth.clone(),
        });
        let catalog = CatalogRepository::new(store.clone());
        let publication = catalog
            .create_publication(NewPublication {
                content_type: libraryd::catalog::ContentType::Magazine,
                title: "Owned magazine".into(),
                sort_title: None,
                run_label: None,
                known_unit_count: None,
            })
            .await
            .unwrap()
            .id;
        let edition = catalog
            .create_edition(NewEdition {
                publication_id: publication.clone(),
                language: "en".into(),
                region: None,
                publisher: None,
            })
            .await
            .unwrap()
            .id;
        Self {
            directory,
            store,
            settings,
            auth,
            app,
            read,
            manage,
            cookie: format!("library_session={}", session.token),
            integration,
            edition,
            requests,
            combined_label,
            server,
        }
    }

    fn path(&self, identifier: &str) -> String {
        format!(
            "/api/v1/search/archive/proposal?integration_id={}&identifier={identifier}&edition_id={}",
            self.integration, self.edition
        )
    }

    fn accept(&self, identifier: &str, label: &str, date: Value, key: &str) -> Value {
        json!({"integration_id":self.integration,"identifier":identifier,"edition_id":self.edition,"label":label,"kind":"issue","date":date,"idempotency_key":key})
    }

    async fn cooldown(&self) {
        let mut tx = self.store.begin_write().await.unwrap();
        sqlx::query("UPDATE search_cooldowns SET next_at = 0")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn stop(self) {
        self.server.abort();
        let _ = self.server.await;
        self.store.close().await;
        drop(self.directory);
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    origin: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if key.starts_with("library_session=") {
        request = request.header(header::COOKIE, key);
    } else if !key.is_empty() {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status.is_success() {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn archive_proposals_enforce_manage_origin_and_existing_edition_before_provider_access() {
    let fixture = Fixture::new().await;
    let accept = fixture.accept("year", "1", json!("2024"), "auth");
    for (key, expected) in [
        ("", StatusCode::UNAUTHORIZED),
        (fixture.read.as_str(), StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            call(
                &fixture.app,
                "GET",
                &fixture.path("year"),
                key,
                None,
                Value::Null
            )
            .await
            .0,
            expected
        );
        assert_eq!(
            call(
                &fixture.app,
                "POST",
                "/api/v1/search/archive/accept",
                key,
                None,
                accept.clone()
            )
            .await
            .0,
            expected
        );
    }
    for origin in [None, Some("http://other.invalid")] {
        assert_eq!(
            call(
                &fixture.app,
                "POST",
                "/api/v1/search/archive/accept",
                &fixture.cookie,
                origin,
                accept.clone()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let missing = fixture.path("year").replace(&fixture.edition, "missing");
    assert_eq!(
        call(
            &fixture.app,
            "GET",
            &missing,
            &fixture.manage,
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut missing = accept.clone();
    missing["edition_id"] = json!("missing");
    assert_eq!(
        call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            missing
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
    let result = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.cookie,
        Some("http://localhost"),
        accept,
    )
    .await;
    assert_eq!(result.0, StatusCode::CREATED, "{}", result.1);
    fixture.stop().await;
}

#[tokio::test]
async fn archive_proposals_preserve_raw_combined_labels_and_exact_date_precision_without_writes() {
    let fixture = Fixture::new().await;
    for (identifier, label, date, precision, combined, raw_dates) in [
        (
            "padded",
            " 9 ",
            json!("2024-03"),
            json!("month"),
            false,
            json!([" 2024-03 "]),
        ),
        (
            "year",
            "1",
            json!("2024"),
            json!("year"),
            false,
            json!(["2024"]),
        ),
        (
            "month",
            "2",
            json!("2024-02"),
            json!("month"),
            false,
            json!(["2024-02"]),
        ),
        (
            "day",
            "5",
            json!("2024-02-29"),
            json!("day"),
            false,
            json!(["2024-02-29"]),
        ),
        (
            "ambiguous",
            "6",
            Value::Null,
            Value::Null,
            false,
            json!(["2024-01", "2024-02"]),
        ),
        (
            "combined",
            "3-4",
            json!("2025-01"),
            json!("month"),
            true,
            json!(["2025-01"]),
        ),
        (
            "text-combined",
            "Jan/Feb",
            Value::Null,
            Value::Null,
            true,
            json!(["January/February 2024"]),
        ),
    ] {
        fixture.cooldown().await;
        let (status, result) = call(
            &fixture.app,
            "GET",
            &fixture.path(identifier),
            &fixture.manage,
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["label"], label);
        assert_eq!(result["raw_labels"], json!([label]));
        assert_eq!(result["raw_dates"], raw_dates);
        assert_eq!(result["date"], date);
        assert_eq!(result["date_precision"], precision);
        assert_eq!(result["combined"], combined);
        assert_eq!(result["kind"], "issue");
        assert_eq!(
            result["source_url"],
            format!("https://archive.org/details/{identifier}")
        );
    }
    for query in [
        "SELECT count(*) FROM units",
        "SELECT count(*) FROM provider_links",
        "SELECT count(*) FROM archive_unit_acceptances",
        "SELECT count(*) FROM jobs",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(fixture.store.reader())
                .await
                .unwrap(),
            0
        );
    }
    fixture.stop().await;
}

#[tokio::test]
async fn archive_acceptance_is_atomic_recomputed_replayable_and_rejects_duplicates_and_invalid_overrides()
 {
    let mut fixture = Fixture::new().await;
    let (status, _) = call(
        &fixture.app,
        "GET",
        &fixture.path("combined"),
        &fixture.manage,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    *fixture.combined_label.lock().unwrap() = "7-8".into();
    let accept = fixture.accept("combined", "3-4", json!("2025-01"), "accept-one");
    for (field, invalid) in [
        ("label", json!("  ")),
        ("date", json!("2023-02-29")),
        ("kind", json!("unknown")),
        ("kind", json!("special")),
        ("raw_labels", json!(["forged"])),
    ] {
        fixture.cooldown().await;
        let mut body = accept.clone();
        body[field] = invalid;
        assert_eq!(
            call(
                &fixture.app,
                "POST",
                "/api/v1/search/archive/accept",
                &fixture.manage,
                None,
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    fixture.cooldown().await;
    let (status, accepted) = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        accept.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{accepted}");
    assert_eq!(accepted["unit"]["label"], "3-4");
    assert_eq!(accepted["unit"]["date_precision"], "month");
    assert_eq!(accepted["unit"]["kind"], "issue");
    assert_eq!(accepted["proposal"]["raw_labels"], json!(["7-8"]));
    let unit_id = accepted["unit"]["id"].as_str().unwrap();
    let link: (String, String, String) =
        sqlx::query_as("SELECT provider, external_id, unit_id FROM provider_links")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap();
    assert_eq!(
        link,
        ("internet_archive".into(), "combined".into(), unit_id.into())
    );
    fixture.app = routes(SearchContext {
        search: Search::new(fixture.store.clone(), fixture.settings.clone()),
        auth: fixture.auth.clone(),
    });
    let requests = fixture.requests.load(Ordering::SeqCst);
    let replay = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        accept.clone(),
    )
    .await;
    assert_eq!(replay, (StatusCode::CREATED, accepted.clone()));
    let mut conflict = accept.clone();
    conflict["label"] = json!("changed");
    let conflict = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        conflict,
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["error"]["code"], "idempotency_conflict");
    assert_eq!(fixture.requests.load(Ordering::SeqCst), requests);
    fixture.cooldown().await;
    let duplicate = call(
        &fixture.app,
        "GET",
        &fixture.path("combined"),
        &fixture.manage,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["existing_unit_id"], unit_id);
    fixture.cooldown().await;
    let mut duplicate = accept;
    duplicate["idempotency_key"] = json!("second-key");
    duplicate["label"] = json!("different label");
    duplicate["date"] = Value::Null;
    let duplicate = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        duplicate,
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["existing_unit_id"], unit_id);
    let catalog = CatalogRepository::new(fixture.store.clone());
    let manual = catalog
        .create_unit(NewUnit {
            edition_id: fixture.edition.clone(),
            label: "  OTHER  ".into(),
            kind: libraryd::catalog::UnitKind::Special,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    fixture.cooldown().await;
    let duplicate = call(
        &fixture.app,
        "GET",
        &fixture.path("other"),
        &fixture.manage,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["existing_unit_id"], manual.id);
    fixture.cooldown().await;
    let duplicate = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        fixture.accept(
            "other",
            "Renamed to bypass duplicate",
            Value::Null,
            "renamed-duplicate",
        ),
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["existing_unit_id"], manual.id);
    let dated = catalog
        .create_unit(NewUnit {
            edition_id: fixture.edition.clone(),
            label: "Different label".into(),
            kind: libraryd::catalog::UnitKind::Issue,
            sort_key: None,
            date: Some("2024-02".into()),
        })
        .await
        .unwrap();
    fixture.cooldown().await;
    let duplicate = call(
        &fixture.app,
        "GET",
        &fixture.path("month"),
        &fixture.manage,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["existing_unit_id"], dated.id);
    fixture.cooldown().await;
    let accepted_day = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        fixture.accept("day", "5", json!("2024-02-29"), "day-key"),
    )
    .await;
    assert_eq!(accepted_day.0, StatusCode::CREATED, "{}", accepted_day.1);
    assert_eq!(accepted_day.1["unit"]["date_precision"], "day");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM archive_unit_acceptances")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap(),
        0
    );
    fixture.stop().await;
}
