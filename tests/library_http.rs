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
async fn inventory_entry_lookup_is_exact_scoped_and_preserves_association_truth() {
    let state = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(state.path()).await.unwrap();
    let auth = AuthService::new(store.clone());
    auth.setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    let manage = auth
        .create_key("manage", Scope::Manage)
        .await
        .unwrap()
        .secret;
    let read = auth.create_key("read", Scope::Read).await.unwrap().secret;
    let app = app::router(store.clone());
    let root_id = uuid::Uuid::new_v4().to_string();
    let other_root_id = uuid::Uuid::new_v4().to_string();
    let source = state.path().join("unavailable-source");
    let mut tx = store.begin_write().await.unwrap();
    for (id, path) in [
        (&root_id, source.clone()),
        (&other_root_id, state.path().join("other-source")),
    ] {
        sqlx::query("INSERT INTO library_roots(id,label,path) VALUES(?,'Fixture library',?)")
            .bind(id)
            .bind(path.to_str().unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for index in 1..=61 {
        sqlx::query("INSERT INTO scan_entries(id,root_id,relative_path,format,signature,size_bytes,mtime_ns,state) VALUES(?,?,?,'cbz','current',1,1,'pending_association')")
            .bind(uuid::Uuid::from_u128(index).to_string())
            .bind(&root_id)
            .bind(format!("{index}.cbz"))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for statement in [
        "INSERT INTO publications(id,content_type,title,sort_title) VALUES('pub','comic','Fixture comic','Fixture comic')",
        "INSERT INTO editions(id,publication_id,language) VALUES('ed','pub','en')",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('file',?,'cbz','current',1)")
        .bind(source.join("61.cbz").to_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    for index in 1..=6 {
        let unit_id = format!("unit-{index}");
        sqlx::query(
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES(?,'ed',?,'issue',?)",
        )
        .bind(&unit_id)
        .bind(index.to_string())
        .bind(index.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('file',?,'user_confirmed')")
            .bind(&unit_id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let path = format!("/api/v1/library/roots/{root_id}/entries");
    let entry_id = uuid::Uuid::from_u128(61).to_string();
    let exact_path = format!("{path}?entry_id={entry_id}");
    let (status, first) = call(&app, &manage, "GET", &path, json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert!(
        first["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["id"] != entry_id)
    );
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = call(
        &app,
        &manage,
        "GET",
        &format!("{path}?cursor={cursor}"),
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["items"].as_array().unwrap().len(), 11);
    assert!(second["next_cursor"].is_null());
    let library = libraryd::library::roots::Library::new(store.clone());
    assert_eq!(
        serde_json::to_value(
            library
                .inventory_entries(&root_id, Some(cursor), 50)
                .await
                .unwrap()
        )
        .unwrap(),
        second
    );
    let (status, exact) = call(&app, &manage, "GET", &exact_path, json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(exact["items"], json!([second["items"][10]]));
    assert!(exact["next_cursor"].is_null());
    assert_eq!(exact["items"][0]["associated_unit_count"], 6);
    let units = exact["items"][0]["associated_units"].as_array().unwrap();
    assert_eq!(units.len(), 5);
    assert_eq!(units[0]["unit_id"], "unit-1");
    assert_eq!(units[4]["unit_id"], "unit-5");
    let (status, canonical) = call(
        &app,
        &manage,
        "GET",
        &format!("{path}?entry_id={}&limit=1", entry_id.to_uppercase()),
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canonical, exact);

    for missing_path in [
        format!("{path}?entry_id={}", uuid::Uuid::new_v4()),
        format!("/api/v1/library/roots/{other_root_id}/entries?entry_id={entry_id}"),
    ] {
        let (status, page) = call(&app, &manage, "GET", &missing_path, json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page, json!({"items": [], "next_cursor": null}));
    }
    assert_eq!(
        call(&app, &read, "GET", &exact_path, json!(null)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, "", "GET", &exact_path, json!(null)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            &manage,
            "GET",
            &format!(
                "/api/v1/library/roots/{}/entries?entry_id={entry_id}",
                uuid::Uuid::new_v4()
            ),
            json!(null)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for query in [
        "entry_id=".to_string(),
        "entry_id=invalid".to_string(),
        "entry_id=%20".to_string(),
        format!("entry_id={entry_id}&cursor={cursor}"),
        format!("entry_id={entry_id}&cursor="),
        format!("entry_id={entry_id}&limit=0"),
        format!("entry_id={entry_id}&limit=101"),
        "cursor=invalid".to_string(),
    ] {
        assert_eq!(
            call(
                &app,
                &manage,
                "GET",
                &format!("{path}?{query}"),
                json!(null)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }

    for (signature, size, state) in [
        ("changed", 1, "pending_association"),
        ("current", 2, "pending_association"),
        ("current", 1, "missing"),
    ] {
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query(
            "UPDATE scan_entries SET signature = ?, size_bytes = ?, state = ? WHERE id = ?",
        )
        .bind(signature)
        .bind(size)
        .bind(state)
        .bind(&entry_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (status, changed) = call(&app, &manage, "GET", &exact_path, json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(changed["items"][0]["associated_unit_count"], 0);
        assert_eq!(changed["items"][0]["associated_units"], json!([]));
    }
    assert!(!source.exists());
    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scan_runs")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(scans, 0);
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(jobs, 0);
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
