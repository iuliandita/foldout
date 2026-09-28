use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use libraryd::{
    app,
    catalog::{
        CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind,
        wanted::WantedRepository,
    },
    providers::{
        ReleaseProtocol,
        release_evidence::{Evidence, parse_release_evidence},
    },
    search::{
        matching::{Candidate, evaluate},
        selection::{Assessment, AssessmentInput, ReleaseIdentity, SelectionRepository},
    },
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "http://127.0.0.1:8787";
const DECISIONS: &str = "/api/v1/search/release-decisions";

async fn post(
    app: &Router,
    path: &str,
    credential: Option<&str>,
    origin: Option<&str>,
    keys: &[&str],
    body: Value,
) -> Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(credential) = credential {
        request = if credential.starts_with("key_") {
            request.header(header::AUTHORIZATION, format!("Bearer {credential}"))
        } else {
            request.header(header::COOKIE, credential)
        };
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    for key in keys {
        request = request.header("idempotency-key", *key);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn assessment(store: &SqliteStore, owner: &str, label: &str) -> Assessment {
    let catalog = CatalogRepository::new(store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type: ContentType::Magazine,
            title: "Monthly".into(),
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
    let unit = catalog
        .create_unit(NewUnit {
            edition_id: edition.id,
            label: label.into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    let target = WantedRepository::new(store.clone())
        .unit_context(&unit.id)
        .await
        .unwrap()
        .unwrap();
    let repository = SelectionRepository::new(store.clone());
    let policy = repository
        .current_or_default_policy(owner, &unit.id)
        .await
        .unwrap();
    let identity = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        integration_id: Uuid::new_v4().to_string(),
        source_fingerprint: "a".repeat(64),
        indexer_id: 7,
        guid_digest: "b".repeat(64),
        content_type: ContentType::Magazine,
        protocol: ReleaseProtocol::Usenet,
    };
    let evidence = parse_release_evidence("Monthly #1 [language:en] [pdf]", ContentType::Magazine);
    let evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &evidence,
        },
    );
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 300;
    repository
        .record_assessment(
            owner,
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity,
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at,
            },
        )
        .await
        .unwrap()
}

async fn fixture() -> (tempfile::TempDir, SqliteStore, Router, String, String) {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let app = app::router(store.clone());
    let setup = post(
        &app,
        "/api/v1/auth/setup",
        None,
        None,
        &[],
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    assert_eq!(setup.status(), StatusCode::CREATED);
    let cookie = setup.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let owner = json(setup).await["user_id"].as_str().unwrap().to_owned();
    (directory, store, app, cookie, owner)
}

#[tokio::test]
async fn decision_routes_enforce_auth_origin_scope_and_single_idempotency_header() {
    let (_dir, store, app, cookie, owner) = fixture().await;
    let item = assessment(&store, &owner, "1").await;
    let body = json!({"assessment_id":item.id,"action":"rejected"});
    assert_eq!(
        post(&app, DECISIONS, None, None, &["reject"], body.clone())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            &app,
            DECISIONS,
            Some(&cookie),
            None,
            &["reject"],
            body.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &app,
            DECISIONS,
            Some(&cookie),
            Some("https://elsewhere.invalid"),
            &["reject"],
            body.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let key = post(
        &app,
        "/api/v1/auth/keys",
        Some(&cookie),
        Some(ORIGIN),
        &[],
        json!({"name":"reader","scope":"read"}),
    )
    .await;
    let key = json(key).await["secret"].as_str().unwrap().to_owned();
    assert_eq!(
        post(&app, DECISIONS, Some(&key), None, &["reject"], body.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for keys in [vec![], vec!["one", "two"], vec!["invalid key"]] {
        assert_eq!(
            post(
                &app,
                DECISIONS,
                Some(&cookie),
                Some(ORIGIN),
                &keys,
                body.clone()
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let response = post(
        &app,
        DECISIONS,
        Some(&cookie),
        Some(ORIGIN),
        &["reject"],
        body.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let first = json(response).await;
    assert_eq!(
        json(
            post(
                &app,
                DECISIONS,
                Some(&cookie),
                Some(ORIGIN),
                &["reject"],
                body
            )
            .await
        )
        .await,
        first
    );
    let revoke = format!("{DECISIONS}/{}/revocations", first["id"].as_str().unwrap());
    assert_eq!(
        post(&app, &revoke, None, None, &["revoke"], json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&app, &revoke, Some(&cookie), None, &["revoke"], json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&app, &revoke, Some(&key), None, &["revoke"], json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &app,
            &revoke,
            Some(&cookie),
            Some(ORIGIN),
            &["revoke"],
            json!({"unexpected":true})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let response = post(
        &app,
        &revoke,
        Some(&cookie),
        Some(ORIGIN),
        &["revoke"],
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let first = json(response).await;
    assert_eq!(
        json(
            post(
                &app,
                &revoke,
                Some(&cookie),
                Some(ORIGIN),
                &["revoke"],
                json!({})
            )
            .await
        )
        .await,
        first
    );
}

#[tokio::test]
async fn decision_http_requires_exact_ack_and_hides_foreign_records_without_enqueuing() {
    let (_dir, store, app, cookie, owner) = fixture().await;
    let item = assessment(&store, &owner, "1").await;
    let request = json!({"assessment_id":item.id,"action":"selected"});
    let response = post(
        &app,
        DECISIONS,
        Some(&cookie),
        Some(ORIGIN),
        &["choose"],
        request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(response).await["error"]["code"],
        "assessment_acknowledgement_required"
    );
    let request =
        json!({"assessment_id":item.id,"action":"selected","acknowledged_assessment_id":item.id});
    assert_eq!(
        post(
            &app,
            DECISIONS,
            Some(&cookie),
            Some(ORIGIN),
            &["choose"],
            request
        )
        .await
        .status(),
        StatusCode::OK
    );
    let mismatch = assessment(&store, &owner, "99").await;
    let response = post(&app, DECISIONS, Some(&cookie), Some(ORIGIN), &["mismatch"], json!({"assessment_id":mismatch.id,"action":"selected","acknowledged_assessment_id":mismatch.id})).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json(response).await["error"]["code"], "release_ineligible");
    let foreign = assessment(&store, "another-owner", "1").await;
    let response = post(
        &app,
        DECISIONS,
        Some(&cookie),
        Some(ORIGIN),
        &["foreign"],
        json!({"assessment_id":foreign.id,"action":"rejected"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(response).await["error"]["code"], "selection_not_found");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisition_intents")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(count, 0);
}
