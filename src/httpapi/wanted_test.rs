use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{app, store::sqlite::SqliteStore};

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    credential: Option<&str>,
    body: Value,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(credential) = credential {
        request = if credential.starts_with("key_") {
            request.header(header::AUTHORIZATION, format!("Bearer {credential}"))
        } else {
            request
                .header(header::COOKIE, credential)
                .header(header::ORIGIN, "http://127.0.0.1:8787")
        };
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn wanted_http_enforces_read_contracts_and_projects_cataloged_units() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let app = app::router(store.clone());
    assert_eq!(
        request(&app, "GET", "/api/v1/wanted", None, json!(null))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let setup = request(
        &app,
        "POST",
        "/api/v1/auth/setup",
        None,
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    assert_eq!(setup.status(), StatusCode::CREATED);
    let session = setup.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let owner = json(setup).await["user_id"].as_str().unwrap().to_owned();
    let response = request(
        &app,
        "POST",
        "/api/v1/auth/keys",
        Some(&session),
        json!({"name":"reader","scope":"read"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let key = json(response).await["secret"].as_str().unwrap().to_owned();

    let mut units = Vec::new();
    for (kind, title, label, count) in [
        ("comic", "100%_\\ Comic", "100%_\\ Unit", 99),
        ("manga", "Manga", "Chapter 1", 0),
        ("magazine", "Magazine", "Issue 1", 0),
    ] {
        let response = request(
            &app,
            "POST",
            "/api/v1/publications",
            Some(&session),
            json!({"content_type":kind,"title":title,"known_unit_count":count}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let publication = json(response).await;
        let response = request(
            &app,
            "POST",
            "/api/v1/editions",
            Some(&session),
            json!({"publication_id":publication["id"],"language":"en"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let edition = json(response).await;
        let response = request(
            &app,
            "POST",
            "/api/v1/units",
            Some(&session),
            json!({"edition_id":edition["id"],"label":label,"kind":"issue"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        units.push((publication, edition, json(response).await));
    }

    let response = request(
        &app,
        "GET",
        "/api/v1/wanted?availability=all",
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let all = json(response).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 3);
    let totals = all["publication_totals"].as_array().unwrap();
    assert_eq!(totals.len(), 3);
    for (index, total) in totals.iter().enumerate() {
        assert_eq!(
            total["publication_id"],
            all["items"][index]["context"]["publication"]["id"]
        );
        assert_eq!(
            total.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "changed",
                "missing",
                "monitored",
                "present",
                "publication_id",
                "scan_problem",
                "total",
                "unverified"
            ]
        );
        assert_eq!((&total["total"], &total["missing"]), (&json!(1), &json!(1)));
    }
    let response = request(
        &app,
        "GET",
        "/api/v1/wanted?availability=all&limit=1",
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let first = json(response).await;
    let cursor = first["next_cursor"].as_str().unwrap();
    let response = request(
        &app,
        "GET",
        &format!("/api/v1/wanted?availability=all&limit=1&cursor={cursor}"),
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_ne!(
        first["items"][0]["context"]["unit"]["id"],
        json(response).await["items"][0]["context"]["unit"]["id"]
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/v1/wanted?availability=all&kind=comic&limit=1&cursor={cursor}"),
            Some(&key),
            json!(null)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let comic_id = units[0].0["id"].as_str().unwrap();
    let comic_unit = units[0].2["id"].as_str().unwrap();
    let response = request(
        &app,
        "GET",
        &format!("/api/v1/wanted?q=100%25_%5C&kind=comic&publication_id={comic_id}"),
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let filtered = json(response).await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["items"][0]["context"]["unit"]["id"], comic_unit);

    let response = request(
        &app,
        "GET",
        &format!("/api/v1/units/{comic_unit}"),
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let context = json(response).await;
    assert_eq!(context["publication"]["id"], comic_id);
    assert_eq!(context["edition"]["id"], units[0].1["id"]);
    assert_eq!(context["unit"]["id"], comic_unit);
    let response = request(
        &app,
        "PATCH",
        &format!("/api/v1/units/{comic_unit}"),
        Some(&session),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut tx = store.begin_write().await.unwrap();
    for statement in [
        "INSERT INTO library_roots(id,label,path) VALUES('root','Root','/library')",
        "INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path) VALUES('changed','/library/changed.cbz','cbz','old',1,'root','changed.cbz')",
        "INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path) VALUES('missing','/library/missing.cbz','cbz','same',1,'root','missing.cbz')",
        "INSERT INTO scan_entries(id,root_id,relative_path,signature,size_bytes,mtime_ns,state) VALUES('scan-changed','root','changed.cbz','new',1,1,'pending_association')",
        "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('scan-missing','root','missing.cbz','missing')",
        "INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('legacy','/legacy.cbz','cbz','same',1)",
        "INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES('integration','comicvine','Source','https://source',1,'{}',1,zeroblob(24),zeroblob(28))",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    for file in ["changed", "missing"] {
        sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES(?,?,'user_confirmed')").bind(file).bind(comic_unit).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('legacy',?,'user_confirmed')").bind(units[1].2["id"].as_str().unwrap()).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('monitor',?,?, 'integration','comic','q',900,'review_only',1,0,'scheduled')").bind(&owner).bind(comic_unit).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    let response = request(
        &app,
        "GET",
        "/api/v1/wanted?availability=attention&monitoring=monitored",
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let attention = json(response).await;
    assert_eq!(attention["items"].as_array().unwrap().len(), 1);
    assert_eq!(attention["items"][0]["counts"]["changed"], 1);
    assert_eq!(attention["items"][0]["counts"]["missing"], 1);
    assert_eq!(attention["publication_totals"].as_array().unwrap().len(), 1);
    assert_eq!(attention["publication_totals"][0]["total"], 1);
    assert_eq!(attention["publication_totals"][0]["monitored"], 1);
    assert_eq!(attention["publication_totals"][0]["changed"], 1);
    let response = request(
        &app,
        "GET",
        "/api/v1/wanted?availability=unverified",
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let unverified = json(response).await;
    assert_eq!(unverified["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        unverified["items"][0]["context"]["unit"]["id"],
        units[1].2["id"]
    );

    for path in [
        "/api/v1/wanted?unexpected=1",
        "/api/v1/wanted?limit=0",
        "/api/v1/wanted?limit=101",
        "/api/v1/wanted?kind=nope",
        "/api/v1/wanted?monitoring=nope",
        "/api/v1/wanted?availability=nope",
        "/api/v1/wanted?cursor=not-a-cursor",
        "/api/v1/wanted?availability=all&limit=1&cursor=not-a-cursor",
    ] {
        assert_eq!(
            request(&app, "GET", path, Some(&key), json!(null))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/units/missing",
            Some(&key),
            json!(null)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn about_requires_read_access_and_reports_only_name_and_version() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
    assert_eq!(
        request(&app, "GET", "/api/v1/about", None, json!(null))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let setup = request(
        &app,
        "POST",
        "/api/v1/auth/setup",
        None,
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    assert_eq!(setup.status(), StatusCode::CREATED);
    let session = setup.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let response = request(
        &app,
        "POST",
        "/api/v1/auth/keys",
        Some(&session),
        json!({"name":"reader","scope":"read"}),
    )
    .await;
    let key = json(response).await["secret"].as_str().unwrap().to_owned();
    let expected = json!({"name":"Foldout","version":env!("CARGO_PKG_VERSION")});
    for credential in [&session, &key] {
        let response = request(&app, "GET", "/api/v1/about", Some(credential), json!(null)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(json(response).await, expected);
    }
}
