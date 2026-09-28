use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use libraryd::{
    app,
    auth::{AuthService, Scope},
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &Router,
    key: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn scan_preview_adoption_and_job_redaction_work_end_to_end() {
    let state = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    let original = std::fs::read("tests/fixtures/natural-order.cbz").unwrap();
    std::fs::write(source.path().join("sample.cbz"), &original).unwrap();
    let store = SqliteStore::open(state.path()).await.unwrap();
    let auth = AuthService::new(store.clone());
    auth.setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    let key = auth.create_key("test", Scope::Admin).await.unwrap().secret;
    let app = app::router(store.clone());
    let (status, root) = call(
        &app,
        &key,
        "POST",
        "/api/v1/library/roots",
        json!({"label":"Fixture library","path":source.path()}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let root_id = root["id"].as_str().unwrap();
    let (status, job) = call(
        &app,
        &key,
        "POST",
        &format!("/api/v1/library/roots/{root_id}/scan"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(job.get("payload").is_none() && job.get("scope").is_none());
    let (_, same) = call(
        &app,
        &key,
        "POST",
        &format!("/api/v1/library/roots/{root_id}/scan"),
        json!({}),
    )
    .await;
    assert_eq!(same["id"], job["id"]);
    let (stop, rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(libraryd::worker::run(store.clone(), rx));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let (_, current) = call(
                &app,
                &key,
                "GET",
                &format!("/api/v1/jobs/{}", job["id"].as_str().unwrap()),
                json!(null),
            )
            .await;
            if current["state"] == "completed" {
                break;
            }
            assert_ne!(current["state"], "failed");
            assert!(
                !worker.is_finished(),
                "worker exited before completion; last job: {current}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    worker.await.unwrap().unwrap();
    let (_, entries) = call(
        &app,
        &key,
        "GET",
        &format!("/api/v1/library/roots/{root_id}/entries"),
        json!(null),
    )
    .await;
    assert_eq!(entries["items"].as_array().unwrap().len(), 1);
    assert_eq!(entries["items"][0]["associated_unit_count"], 0);
    assert_eq!(entries["items"][0]["associated_units"], json!([]));
    let (_, publication) = call(
        &app,
        &key,
        "POST",
        "/api/v1/publications",
        json!({"content_type":"comic","title":"Fixture comic"}),
    )
    .await;
    let (_, edition) = call(
        &app,
        &key,
        "POST",
        "/api/v1/editions",
        json!({"publication_id":publication["id"],"language":"en"}),
    )
    .await;
    let (_, unit) = call(
        &app,
        &key,
        "POST",
        "/api/v1/units",
        json!({"edition_id":edition["id"],"label":"1","kind":"issue"}),
    )
    .await;
    let (status, preview) = call(
        &app,
        &key,
        "POST",
        "/api/v1/library/previews",
        json!({"entry_id":entries["items"][0]["id"],"unit_id":unit["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let path = format!(
        "/api/v1/library/previews/{}/accept",
        preview["id"].as_str().unwrap()
    );
    let (status, file) = call(&app, &key, "POST", &path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(file.get("path").is_none());
    let (_, repeat) = call(&app, &key, "POST", &path, json!({})).await;
    assert_eq!(repeat["id"], file["id"]);
    assert_eq!(
        std::fs::read(source.path().join("sample.cbz")).unwrap(),
        original
    );
    let inventory_path = format!("/api/v1/library/roots/{root_id}/entries");
    let (status, associated) = call(&app, &key, "GET", &inventory_path, json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(associated["items"][0]["associated_unit_count"], 1);
    assert_eq!(
        associated["items"][0]["associated_units"],
        json!([{
            "publication_id": publication["id"],
            "publication_title": "Fixture comic",
            "unit_id": unit["id"],
            "unit_label": "1",
            "unit_kind": "issue"
        }])
    );
    assert_eq!(
        associated["items"][0]["state"],
        entries["items"][0]["state"]
    );
    assert_eq!(
        associated["items"][0]["reason"],
        entries["items"][0]["reason"]
    );
    let (_, second_unit) = call(
        &app,
        &key,
        "POST",
        "/api/v1/units",
        json!({"edition_id":edition["id"],"label":"2","kind":"issue"}),
    )
    .await;
    let (status, second_preview) = call(
        &app,
        &key,
        "POST",
        "/api/v1/library/previews",
        json!({"entry_id":entries["items"][0]["id"],"unit_id":second_unit["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        &app,
        &key,
        "POST",
        &format!(
            "/api/v1/library/previews/{}/accept",
            second_preview["id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let library = libraryd::library::roots::Library::new(store.clone());
    library.scan(root_id).await.unwrap();
    let (_, rescanned) = call(&app, &key, "GET", &inventory_path, json!(null)).await;
    assert_eq!(rescanned["items"][0]["associated_unit_count"], 2);
    std::fs::write(source.path().join("sample.cbz"), b"changed content").unwrap();
    library.scan(root_id).await.unwrap();
    let (_, changed) = call(&app, &key, "GET", &inventory_path, json!(null)).await;
    assert_eq!(changed["items"][0]["associated_unit_count"], 0);
    std::fs::remove_file(source.path().join("sample.cbz")).unwrap();
    library.scan(root_id).await.unwrap();
    let (_, missing) = call(&app, &key, "GET", &inventory_path, json!(null)).await;
    assert_eq!(missing["items"][0]["state"], "missing");
    assert_eq!(missing["items"][0]["associated_unit_count"], 0);
    let read = auth.create_key("read", Scope::Read).await.unwrap().secret;
    assert_eq!(
        call(
            &app,
            &read,
            "POST",
            &format!("/api/v1/library/roots/{root_id}/scan"),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn scan_jobs_name_their_root_without_exposing_paths_below_admin() {
    let state = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(state.path()).await.unwrap();
    let auth = AuthService::new(store.clone());
    auth.setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    let admin = auth.create_key("admin", Scope::Admin).await.unwrap().secret;
    let read = auth.create_key("read", Scope::Read).await.unwrap().secret;
    let app = app::router(store.clone());
    let (_, root) = call(
        &app,
        &admin,
        "POST",
        "/api/v1/library/roots",
        json!({"label":"Fixture library","path":source.path()}),
    )
    .await;
    let root_id = root["id"].as_str().unwrap();
    let (status, job) = call(
        &app,
        &admin,
        "POST",
        &format!("/api/v1/library/roots/{root_id}/scan"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        job["subject"],
        json!({"kind":"library_root","root_id":root_id,"root_label":null})
    );
    let path = source.path().to_str().unwrap();
    for (key, label) in [(&admin, json!("Fixture library")), (&read, json!(null))] {
        let expected = json!({"kind":"library_root","root_id":root_id,"root_label":label});
        let (status, list) = call(&app, key, "GET", "/api/v1/jobs", json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["items"][0]["subject"], expected);
        let (status, detail) = call(
            &app,
            key,
            "GET",
            &format!("/api/v1/jobs/{}", job["id"].as_str().unwrap()),
            json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["subject"], expected);
        assert!(!list.to_string().contains(path) && !detail.to_string().contains(path));
    }
}
