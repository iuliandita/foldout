use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use libraryd::{app, store::sqlite::SqliteStore};
use serde_json::{Value, json};
use tower::ServiceExt;

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
async fn catalog_http_creates_all_content_types_and_enforces_access_and_cursors() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
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

    let mut comic_ids = Vec::new();
    for (content_type, title) in [
        ("comic", "Alpha"),
        ("comic", "Beta"),
        ("manga", "Manga"),
        ("magazine", "Magazine"),
    ] {
        let response = request(&app, "POST", "/api/v1/publications", Some(&session), json!({"content_type":content_type,"title":title,"run_label":"run","known_unit_count":0})).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = json(response).await;
        if content_type == "comic" {
            comic_ids.push(created["id"].as_str().unwrap().to_owned());
        }
    }

    let first = request(
        &app,
        "GET",
        "/api/v1/publications?kind=comic&limit=1",
        Some(&session),
        json!(null),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first = json(first).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = request(
        &app,
        "GET",
        &format!("/api/v1/publications?kind=comic&limit=1&cursor={cursor}"),
        Some(&session),
        json!(null),
    )
    .await;
    let second = json(second).await;
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);

    let key = request(
        &app,
        "POST",
        "/api/v1/auth/keys",
        Some(&session),
        json!({"name":"reader","scope":"read"}),
    )
    .await;
    assert_eq!(key.status(), StatusCode::CREATED);
    let key = json(key).await["secret"].as_str().unwrap().to_owned();
    let link =
        json!({"provider":"comicvine","external_id":"4050-123","publication_id":comic_ids[0]});
    for (credential, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(key.as_str()), StatusCode::FORBIDDEN),
        (Some(session.as_str()), StatusCode::CREATED),
        (Some(session.as_str()), StatusCode::CONFLICT),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/provider-links",
                credential,
                link.clone()
            )
            .await
            .status(),
            expected
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/provider-links",
            Some(&session),
            json!({"provider":"comicvine","external_id":"4050-124"})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "GET", "/api/v1/publications", Some(&key), json!(null))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/publications",
            Some(&key),
            json!({"content_type":"comic","title":"Denied"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    let patched = request(
        &app,
        "PATCH",
        &format!("/api/v1/publications/{}", comic_ids[0]),
        Some(&session),
        json!({"run_label":null,"known_unit_count":null}),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let patched = json(patched).await;
    assert!(patched["run_label"].is_null() && patched["known_unit_count"].is_null());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/publications/missing",
            Some(&session),
            json!(null)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn catalog_http_updates_editions_and_units() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
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
    assert_eq!(response.status(), StatusCode::CREATED);
    let key = json(response).await["secret"].as_str().unwrap().to_owned();
    let response = request(
        &app,
        "POST",
        "/api/v1/publications",
        Some(&session),
        json!({"content_type":"comic","title":"Mutable"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let publication = json(response).await;
    let response = request(&app, "POST", "/api/v1/editions", Some(&session), json!({"publication_id":publication["id"],"language":"en","region":"GB","publisher":"Press"})).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let edition = json(response).await;
    let response = request(&app, "POST", "/api/v1/units", Some(&session), json!({"edition_id":edition["id"],"label":"1","kind":"issue","sort_key":"001","date":"2024"})).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let unit = json(response).await;
    let edition_path = format!("/api/v1/editions/{}", edition["id"].as_str().unwrap());
    assert_eq!(
        request(&app, "PATCH", &edition_path, None, json!({"language":"de"}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &edition_path,
            Some(&key),
            json!({"language":"de"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = request(
        &app,
        "PATCH",
        &edition_path,
        Some(&session),
        json!({"language":"de","region":null}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let changed = json(response).await;
    assert_eq!(changed["publication_id"], publication["id"]);
    assert_eq!(changed["publisher"], "Press");
    let unit_path = format!("/api/v1/units/{}", unit["id"].as_str().unwrap());
    let response = request(
        &app,
        "PATCH",
        &unit_path,
        Some(&session),
        json!({"label":"2","kind":"volume","sort_key":null,"date":"2024-02-29"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let changed = json(response).await;
    assert_eq!(changed["edition_id"], edition["id"]);
    assert_eq!(changed["date_precision"], "day");
    for (path, body, expected) in [
        (
            edition_path.as_str(),
            json!({"language":null}),
            StatusCode::BAD_REQUEST,
        ),
        (
            unit_path.as_str(),
            json!({"date":"2024-02-30"}),
            StatusCode::BAD_REQUEST,
        ),
        ("/api/v1/units/missing", json!({}), StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            request(&app, "PATCH", path, Some(&session), body)
                .await
                .status(),
            expected
        );
    }
}

#[tokio::test]
async fn catalog_search_http_filters_literal_queries_and_preserves_read_authorization() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let app = app::router(SqliteStore::open(directory.path()).await.unwrap());
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
    let key = request(
        &app,
        "POST",
        "/api/v1/auth/keys",
        Some(&session),
        json!({"name":"lookup","scope":"read"}),
    )
    .await;
    assert_eq!(key.status(), StatusCode::CREATED);
    let key = json(key).await["secret"].as_str().unwrap().to_owned();
    let mut publications = Vec::new();
    for (kind, title, run) in [
        ("comic", "Same", "100%_\\"),
        ("comic", "Same", "second 100%_\\"),
        ("manga", "Same", "100%_\\"),
        ("comic", "Other", "100xx"),
    ] {
        let response = request(
            &app,
            "POST",
            "/api/v1/publications",
            Some(&session),
            json!({"content_type":kind,"title":title,"run_label":run}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        publications.push(json(response).await["id"].as_str().unwrap().to_owned());
    }
    let path = "/api/v1/publications?kind=comic&limit=1&q=100%25_%5C";
    assert_eq!(
        request(&app, "GET", path, None, json!(null)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let response = request(&app, "GET", path, Some(&key), json!(null)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let first = json(response).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    let response = request(
        &app,
        "GET",
        &format!("{path}&cursor={cursor}"),
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let second = json(response).await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    assert!(second["next_cursor"].is_null());
    for suffix in ["q=Same&kind=comic", "q=100%25_%5C&kind=manga", "kind=comic"] {
        assert_eq!(
            request(
                &app,
                "GET",
                &format!("/api/v1/publications?{suffix}&cursor={cursor}"),
                Some(&key),
                json!(null)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let response = request(
        &app,
        "GET",
        "/api/v1/publications?q=%20sAmE%20",
        Some(&key),
        json!(null),
    )
    .await;
    assert_eq!(json(response).await["items"].as_array().unwrap().len(), 3);
    let edition = request(&app, "POST", "/api/v1/editions", Some(&session), json!({"publication_id":publications[0],"language":"en","region":"GB","publisher":"Press%_\\"})).await;
    assert_eq!(edition.status(), StatusCode::CREATED);
    let edition = json(edition).await["id"].as_str().unwrap().to_owned();
    for label in ["12.5%_\\", "12x5xxx"] {
        let response = request(
            &app,
            "POST",
            "/api/v1/units",
            Some(&session),
            json!({"edition_id":edition,"label":label,"kind":"issue"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let editions_path = format!("/api/v1/publications/{}/editions", publications[0]);
    let units_path = format!("/api/v1/editions/{edition}/units");
    for (path, q, count) in [
        (&editions_path, "press%25_%5C", 1),
        (&editions_path, "gB", 1),
        (&editions_path, "EN", 1),
        (&units_path, "12.5%25_%5C", 1),
        (&units_path, "missing", 0),
    ] {
        let response = request(
            &app,
            "GET",
            &format!("{path}?q={q}"),
            Some(&key),
            json!(null),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json(response).await["items"].as_array().unwrap().len(),
            count
        );
        assert_eq!(
            request(&app, "GET", &format!("{path}?q={q}"), None, json!(null))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for path in ["/api/v1/publications", &editions_path, &units_path] {
        for q in ["x".repeat(257), "%00".into(), "%0A".into()] {
            let response = request(
                &app,
                "GET",
                &format!("{path}?q={q}"),
                Some(&key),
                json!(null),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}

#[tokio::test]
async fn catalog_http_sorts_filters_availability_and_unit_kind() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let app = app::router(store.clone());
    let setup = request(
        &app,
        "POST",
        "/api/v1/auth/setup",
        None,
        json!({"username":"owner","password":"correct horse battery staple"}),
    )
    .await;
    let session = setup.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut ids = Vec::new();
    for (title, created_at) in [("Alpha", 100), ("Beta", 300), ("Gamma", 200)] {
        let response = request(
            &app,
            "POST",
            "/api/v1/publications",
            Some(&session),
            json!({"content_type":"comic","title":title}),
        )
        .await;
        let id = json(response).await["id"].as_str().unwrap().to_owned();
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("UPDATE publications SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        ids.push(id);
    }
    let first = json(
        request(
            &app,
            "GET",
            "/api/v1/publications?sort=recently_added&limit=2",
            Some(&session),
            json!(null),
        )
        .await,
    )
    .await;
    assert_eq!(first["items"][0]["id"], json!(ids[1]));
    assert_eq!(first["items"][1]["id"], json!(ids[2]));
    let cursor = first["next_cursor"].as_str().unwrap();
    let rest = json(
        request(
            &app,
            "GET",
            &format!("/api/v1/publications?sort=recently_added&limit=2&cursor={cursor}"),
            Some(&session),
            json!(null),
        )
        .await,
    )
    .await;
    assert_eq!(rest["items"][0]["id"], json!(ids[0]));
    assert!(rest["next_cursor"].is_null());
    for path in [
        format!("/api/v1/publications?limit=2&cursor={cursor}"),
        format!("/api/v1/publications?sort=title&limit=2&cursor={cursor}"),
        "/api/v1/publications?sort=newest".into(),
        "/api/v1/publications?availability=some".into(),
    ] {
        let response = request(&app, "GET", &path, Some(&session), json!(null)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }
    for (availability, count) in [("has_files", 0), ("no_files", 3), ("all", 3)] {
        let page = json(
            request(
                &app,
                "GET",
                &format!("/api/v1/publications?availability={availability}"),
                Some(&session),
                json!(null),
            )
            .await,
        )
        .await;
        assert_eq!(
            page["items"].as_array().unwrap().len(),
            count,
            "{availability}"
        );
    }
    let edition = json(
        request(
            &app,
            "POST",
            "/api/v1/editions",
            Some(&session),
            json!({"publication_id":ids[0],"language":"en"}),
        )
        .await,
    )
    .await;
    let edition = edition["id"].as_str().unwrap();
    for (label, kind) in [("1", "issue"), ("2", "volume"), ("3", "issue")] {
        let response = request(
            &app,
            "POST",
            "/api/v1/units",
            Some(&session),
            json!({"edition_id":edition,"label":label,"kind":kind}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let first = json(
        request(
            &app,
            "GET",
            &format!("/api/v1/editions/{edition}/units?kind=issue&limit=1"),
            Some(&session),
            json!(null),
        )
        .await,
    )
    .await;
    assert_eq!(first["items"][0]["label"], "1");
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = json(
        request(
            &app,
            "GET",
            &format!("/api/v1/editions/{edition}/units?kind=issue&limit=1&cursor={cursor}"),
            Some(&session),
            json!(null),
        )
        .await,
    )
    .await;
    assert_eq!(second["items"][0]["label"], "3");
    assert!(second["next_cursor"].is_null());
    for path in [
        format!("/api/v1/editions/{edition}/units?limit=1&cursor={cursor}"),
        format!("/api/v1/editions/{edition}/units?kind=comic"),
    ] {
        let response = request(&app, "GET", &path, Some(&session), json!(null)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }
}
