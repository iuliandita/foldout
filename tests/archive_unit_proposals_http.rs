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
    publication: String,
    race_started: Arc<tokio::sync::Notify>,
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
        let race_started = Arc::new(tokio::sync::Notify::new());
        let started = race_started.clone();
        let race_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let provider = Router::new().route("/metadata/{identifier}", get(move |Path(identifier): Path<String>| {
            let count = count.clone();
            let raw_label = raw_label.clone();
            let started = started.clone();
            let race_barrier = race_barrier.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                if identifier == "race" {
                    started.notify_one();
                    race_barrier.wait().await;
                }
                let (issue, date) = match identifier.as_str() {
                    "volume-one" | "volume-two" => (" 1 ".to_string(), json!("2024-03")),
                    "annual" => ("Annual".to_string(), json!("2024-03-01")),
                    "padded" => (" 9 ".to_string(), json!(" 2024-03 ")),
                    "year" => ("1".to_string(), json!("2024")),
                    "month" => ("2".to_string(), json!("2024-02")),
                    "day" => ("5".to_string(), json!("2024-02-29")),
                    "ambiguous" => ("6".to_string(), json!(["2024-01", "2024-02"])),
                    "text-combined" => ("Jan/Feb".to_string(), json!("January/February 2024")),
                    "combined" => (raw_label.lock().unwrap().clone(), json!("2025-01")),
                    _ => ("other".to_string(), json!(null)),
                };
                Json(json!({"metadata":{"identifier":identifier,"title":"Owned magazine","mediatype":"texts","collection":["magazine_rack"],"issue":issue,"date":date,"volume":match identifier.as_str() { "volume-one" => json!(" 1 "), "volume-two" => json!(" 2 "), _ => Value::Null }},"files":[]}))
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
            publication,
            race_started,
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
    let catalog = CatalogRepository::new(fixture.store.clone());
    for content_type in [
        libraryd::catalog::ContentType::Comic,
        libraryd::catalog::ContentType::Manga,
    ] {
        let publication = catalog
            .create_publication(NewPublication {
                content_type,
                title: "Other publication".into(),
                sort_title: None,
                run_label: None,
                known_unit_count: None,
            })
            .await
            .unwrap();
        let edition = catalog
            .create_edition(NewEdition {
                publication_id: publication.id,
                language: "en".into(),
                region: None,
                publisher: None,
            })
            .await
            .unwrap();
        let path = fixture.path("year").replace(&fixture.edition, &edition.id);
        assert_eq!(
            call(
                &fixture.app,
                "GET",
                &path,
                &fixture.manage,
                None,
                Value::Null
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let mut body = accept.clone();
        body["edition_id"] = json!(edition.id);
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
            "9",
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
        assert_eq!(
            result["raw_labels"],
            if identifier == "padded" {
                json!([" 9 "])
            } else {
                json!([label])
            }
        );
        assert_eq!(result["raw_dates"], raw_dates);
        assert_eq!(result["date"], date);
        assert_eq!(result["date_precision"], precision);
        assert_eq!(result["combined"], combined);
        assert_eq!(result["kind"], if combined { "combined" } else { "issue" });
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
    assert_eq!(duplicate.0, StatusCode::CREATED, "{}", duplicate.1);
    assert_eq!(duplicate.1["unit"]["label"], "Renamed to bypass duplicate");
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
    assert_eq!(duplicate.0, StatusCode::OK);
    assert_eq!(duplicate.1["date_warnings"][0]["id"], dated.id);
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
        3
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

#[tokio::test]
async fn volumes_and_advisory_dates_allow_distinct_issues_in_the_same_month() {
    let fixture = Fixture::new().await;
    let mut ids = Vec::new();
    for (identifier, label, volume) in [
        ("volume-one", "Vol 1 No 1", " 1 "),
        ("volume-two", "Vol 2 No 1", " 2 "),
    ] {
        fixture.cooldown().await;
        let (status, proposal) = call(
            &fixture.app,
            "GET",
            &fixture.path(identifier),
            &fixture.manage,
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{proposal}");
        assert_eq!(proposal["label"], label);
        assert_eq!(proposal["raw_labels"], json!([" 1 "]));
        assert_eq!(proposal["raw_volumes"], json!([volume]));
        assert!(!proposal["combined"].as_bool().unwrap());
        if !ids.is_empty() {
            assert_eq!(proposal["date_warnings"][0]["id"], ids[0]);
        }
        fixture.cooldown().await;
        let result = call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            fixture.accept(identifier, label, json!("2024-03"), identifier),
        )
        .await;
        assert_eq!(result.0, StatusCode::CREATED, "{}", result.1);
        ids.push(result.1["unit"]["id"].clone());
    }
    fixture.cooldown().await;
    let (status, proposal) = call(
        &fixture.app,
        "GET",
        &fixture.path("annual"),
        &fixture.manage,
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{proposal}");
    assert_eq!(proposal["date_warnings"].as_array().unwrap().len(), 2);
    fixture.cooldown().await;
    let mut annual = fixture.accept("annual", "Annual", json!("2024-03-01"), "annual");
    annual["kind"] = json!("special");
    assert_eq!(
        call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            annual
        )
        .await
        .0,
        StatusCode::CREATED
    );
    fixture.cooldown().await;
    let mut combined = fixture.accept("combined", "3-4", json!("2025-01"), "combined-kind");
    combined["kind"] = json!("combined");
    let combined = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        combined,
    )
    .await;
    assert_eq!(combined.0, StatusCode::CREATED, "{}", combined.1);
    assert_eq!(combined.1["unit"]["kind"], "combined");
    assert_eq!(combined.1["proposal"]["raw_labels"], json!(["3-4"]));
    fixture.stop().await;
}

#[tokio::test]
async fn concurrent_accepts_replay_inside_the_transaction_and_receipts_protect_unit_lifetime() {
    let fixture = Fixture::new().await;
    let body = fixture.accept("year", "1", json!("2024"), "concurrent");
    let (first, second) = tokio::join!(
        call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            body.clone()
        ),
        call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            body.clone()
        ),
    );
    assert_eq!(first.0, StatusCode::CREATED, "{}", first.1);
    assert_eq!(first, second);
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);

    let service = Search::new(fixture.store.clone(), fixture.settings.clone());
    let other_owner = service
        .accept_archive_unit(
            "different-owner",
            serde_json::from_value(body.clone()).unwrap(),
        )
        .await;
    assert!(matches!(
        other_owner,
        Err(libraryd::search::ArchiveProposalError::IdempotencyConflict)
    ));
    let catalog = CatalogRepository::new(fixture.store.clone());
    let other_edition = catalog
        .create_edition(NewEdition {
            publication_id: fixture.publication.clone(),
            language: "es".into(),
            region: None,
            publisher: None,
        })
        .await
        .unwrap();
    let mut different_edition = body.clone();
    different_edition["edition_id"] = json!(other_edition.id);
    let result = call(
        &fixture.app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        different_edition,
    )
    .await;
    assert_eq!(result.0, StatusCode::CONFLICT);
    assert_eq!(result.1["error"]["code"], "idempotency_conflict");

    fixture.cooldown().await;
    let race = fixture.accept("race", "Race issue", Value::Null, "transaction-race");
    let app = fixture.app.clone();
    let key = fixture.manage.clone();
    let first_body = race.clone();
    let first = tokio::spawn(async move {
        call(
            &app,
            "POST",
            "/api/v1/search/archive/accept",
            &key,
            None,
            first_body,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.race_started.notified(),
    )
    .await
    .unwrap();
    fixture.cooldown().await;
    let other_app = routes(SearchContext {
        search: Search::new(fixture.store.clone(), fixture.settings.clone()),
        auth: fixture.auth.clone(),
    });
    let second = call(
        &other_app,
        "POST",
        "/api/v1/search/archive/accept",
        &fixture.manage,
        None,
        race.clone(),
    )
    .await;
    let first = first.await.unwrap();
    assert_eq!(first.0, StatusCode::CREATED, "{}", first.1);
    assert_eq!(first, second);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM archive_unit_acceptances")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM units")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap(),
        2
    );
    let unit_id = first.1["unit"]["id"].as_str().unwrap();
    let link: String = sqlx::query_scalar("SELECT id FROM provider_links WHERE unit_id = ?")
        .bind(unit_id)
        .fetch_one(fixture.store.reader())
        .await
        .unwrap();
    catalog.delete_provider_link(&link).await.unwrap();
    assert!(
        matches!(catalog.delete_unit(unit_id).await, Err(libraryd::catalog::CatalogError::Dependencies(message)) if message.contains("archive_unit_acceptances"))
    );
    let mut tx = fixture.store.begin_write().await.unwrap();
    assert!(
        sqlx::query("DELETE FROM units WHERE id = ?")
            .bind(unit_id)
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        call(
            &fixture.app,
            "POST",
            "/api/v1/search/archive/accept",
            &fixture.manage,
            None,
            race
        )
        .await,
        first
    );
    fixture.stop().await;
}
