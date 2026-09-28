use std::{os::unix::fs::MetadataExt, path::Path};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use libraryd::{auth, httpapi, reader, store};

use auth::{AuthService, Scope};
use httpapi::{
    auth::AuthContext,
    reader::{ReaderContext, routes},
};
use reader::service::ReaderService;
use store::sqlite::SqliteStore;

const ORIGIN: &str = "http://127.0.0.1:8787";

#[tokio::test]
async fn metadata_hints_require_management_scope_and_do_not_change_catalog() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let path = "/api/v1/library/files/comic/metadata";
    assert_eq!(
        fixture
            .request("GET", path, None, Value::Null)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .request("GET", path, Some(&fixture.read), Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = fixture
        .request("GET", path, Some(&fixture.manage), Value::Null)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        json!({"file_id":"comic", "hints":null})
    );
    let label: String = sqlx::query_scalar("SELECT label FROM units WHERE id='unit'")
        .fetch_one(fixture.store.reader())
        .await
        .unwrap();
    assert_eq!(label, "1");
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
    store: SqliteStore,
    app: Router,
    read: String,
    manage: String,
    cookie: String,
    user_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("library");
        std::fs::create_dir(&root).unwrap();
        let store = SqliteStore::open(&directory.path().join("state"))
            .await
            .unwrap();
        let auth = AuthService::new(store.clone());
        let login = auth
            .setup("owner", "correct horse battery staple")
            .await
            .unwrap();
        let read = auth.create_key("reader", Scope::Read).await.unwrap().secret;
        let manage = auth
            .create_key("manager", Scope::Manage)
            .await
            .unwrap()
            .secret;
        let app = routes(ReaderContext {
            service: ReaderService::new(store.clone()),
            auth: AuthContext {
                service: auth,
                origin: ORIGIN.into(),
            },
        });
        let mut tx = store.begin_write().await.unwrap();
        for statement in [
            "INSERT INTO publications(id,content_type,title,sort_title) VALUES('publication','magazine','Fixture','Fixture')",
            "INSERT INTO editions(id,publication_id,language) VALUES('edition','publication','en')",
            "INSERT INTO units(id,edition_id,label,kind) VALUES('unit','edition','1','issue')",
            "INSERT INTO units(id,edition_id,label,kind) VALUES('empty','edition','2','issue')",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        sqlx::query("INSERT INTO library_roots(id,label,path) VALUES('root','Fixture',?)")
            .bind(root.to_str().unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        Self {
            _directory: directory,
            root,
            store,
            app,
            read,
            manage,
            cookie: format!("library_session={}", login.token),
            user_id: login.principal.user_id,
        }
    }

    async fn file(&self, id: &str, source: &str, format: &str) -> String {
        let target = self.root.join(format!("{id}.{format}"));
        std::fs::copy(source, &target).unwrap();
        let signature = signature(&target);
        let metadata = target.metadata().unwrap();
        let mut tx = self.store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path,mtime_ns) VALUES(?,?,?,?,?,'root',?,?)")
            .bind(id).bind(target.to_str().unwrap()).bind(format).bind(&signature).bind(metadata.len() as i64)
            .bind(target.file_name().unwrap().to_str().unwrap()).bind(mtime(&metadata)).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES(?,'unit','user_confirmed')")
            .bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        signature
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        input: Value,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(input.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .request("GET", path, Some(&self.read), Value::Null)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        body(response).await
    }
}

fn signature(path: &Path) -> String {
    Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec()
}
async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn cbz_manifest_pages_and_safe_unit_files_require_authentication() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    for path in [
        "/api/v1/library/files/comic/manifest",
        "/api/v1/library/files/comic/pages/0",
        "/api/v1/library/files/comic/progress",
        "/api/v1/units/unit/files",
    ] {
        let response = fixture.request("GET", path, None, Value::Null).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let manifest = fixture.get("/api/v1/library/files/comic/manifest").await;
    assert_eq!(
        manifest,
        json!({"file_id":"comic","signature":signature,"format":"cbz","page_count":3})
    );
    let decoder = reader::archive::ArchiveDecoder::new();
    for (page, name) in ["1.png", "2.png", "10.png"].into_iter().enumerate() {
        let response = fixture
            .request(
                "GET",
                &format!("/api/v1/library/files/comic/pages/{page}"),
                Some(&fixture.read),
                Value::Null,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            bytes.as_ref(),
            decoder
                .page(Path::new("tests/fixtures/natural-order.cbz"), name)
                .await
                .unwrap()
                .bytes
        );
    }
    for page in ["3", "10000", "-1", "hello"] {
        let response = fixture
            .request(
                "GET",
                &format!("/api/v1/library/files/comic/pages/{page}"),
                Some(&fixture.read),
                Value::Null,
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body(response).await["error"]["code"].is_string());
    }
    let files = fixture.get("/api/v1/units/unit/files").await;
    assert_eq!(
        files["items"],
        json!([{"id":"comic","format":"cbz","signature":signature,"size_bytes":549}])
    );
    assert!(!files.to_string().contains(fixture.root.to_str().unwrap()));
    assert_eq!(
        fixture.get("/api/v1/units/empty/files").await,
        json!({"items":[]})
    );
    for path in [
        "/api/v1/library/files/missing/manifest",
        "/api/v1/units/missing/files",
    ] {
        assert_eq!(
            fixture
                .request("GET", path, Some(&fixture.read), Value::Null)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn pdf_manifest_and_bounded_png_page_are_consistent() {
    let fixture = Fixture::new().await;
    fixture
        .file("magazine", "tests/fixtures/single-page.pdf", "pdf")
        .await;
    assert_eq!(
        fixture.get("/api/v1/library/files/magazine/manifest").await["page_count"],
        1
    );
    let response = fixture
        .request(
            "GET",
            "/api/v1/library/files/magazine/pages/0",
            Some(&fixture.read),
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.len() < 32 * 1024 * 1024);
    let image = image::load_from_memory(&bytes).unwrap();
    assert!(image.width() <= 2000 && image.height() <= 2000);
    assert!(image.width() > 0 && image.height() > 0);
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/magazine/pages/1",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn progress_is_revision_checked_file_scoped_and_requires_manage() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    fixture
        .file("other", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let path = "/api/v1/library/files/comic/progress";
    assert_eq!(fixture.get(path).await["revision"], 0);
    let input = json!({"signature":signature,"page":2,"direction":"rtl","revision":0});
    for (token, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(fixture.read.as_str()), StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            fixture
                .request("PUT", path, token, input.clone())
                .await
                .status(),
            status
        );
    }
    let response = fixture
        .request("PUT", path, Some(&fixture.manage), input.clone())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["revision"], 1);
    assert_eq!(
        fixture
            .request("PUT", path, Some(&fixture.manage), input)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let progress = fixture.get(path).await;
    assert_eq!(progress["page"], 2);
    assert_eq!(progress["direction"], "rtl");
    assert_eq!(
        fixture.get("/api/v1/library/files/other/progress").await["page"],
        0
    );
    for input in [
        json!({"signature":signature,"page":3,"direction":"ltr","revision":1}),
        json!({"signature":signature,"page":0,"direction":"diagonal","revision":1}),
        json!({"signature":signature,"page":-1,"direction":"ltr","revision":1}),
        json!({"signature":signature,"page":0,"direction":"ltr","revision":1,"user_id":"someone"}),
    ] {
        assert_eq!(
            fixture
                .request("PUT", path, Some(&fixture.manage), input)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let input = json!({"signature":signature,"page":1,"direction":"vertical","revision":1});
    let request = Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &fixture.cookie)
        .body(Body::from(input.to_string()))
        .unwrap();
    assert_eq!(
        fixture.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &fixture.cookie)
        .header(header::ORIGIN, ORIGIN)
        .body(Body::from(input.to_string()))
        .unwrap();
    assert_eq!(
        fixture.app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
    let user: String =
        sqlx::query_scalar("SELECT user_id FROM reading_progress WHERE file_id='comic'")
            .fetch_one(fixture.store.reader())
            .await
            .unwrap();
    assert_eq!(user, fixture.user_id);
    // Reopening the service must preserve the saved state.
    assert_eq!(
        ReaderService::new(fixture.store.clone())
            .progress(&fixture.user_id, "comic")
            .await
            .unwrap()
            .revision,
        2
    );
}

#[tokio::test]
async fn concurrent_progress_writes_have_one_winner() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let input = json!({"signature":signature,"page":1,"direction":"ltr","revision":0});
    let path = "/api/v1/library/files/comic/progress";
    let (a, b) = tokio::join!(
        fixture.request("PUT", path, Some(&fixture.manage), input.clone()),
        fixture.request("PUT", path, Some(&fixture.manage), input)
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
}

#[tokio::test]
async fn changed_metadata_invalidates_cache_and_changed_signature_requires_reset() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let path = "/api/v1/library/files/comic/progress";
    let input = json!({"signature":signature,"page":2,"direction":"rtl","revision":0});
    assert_eq!(
        fixture
            .request("PUT", path, Some(&fixture.manage), input)
            .await
            .status(),
        StatusCode::OK
    );
    let source = fixture.root.join("comic.cbz");
    std::fs::copy("tests/fixtures/single-page.pdf", &source).unwrap();
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/pages/0",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let metadata = source.metadata().unwrap();
    let new_signature = signature_for(&source);
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET format='pdf',signature=?,size_bytes=?,mtime_ns=? WHERE id='comic'")
        .bind(&new_signature).bind(metadata.len() as i64).bind(mtime(&metadata)).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let progress = fixture.get(path).await;
    assert_eq!(progress["reset_required"], true);
    assert_eq!(progress["page"], 0);
    assert_eq!(progress["revision"], 1);
    let mut input = json!({"signature":new_signature,"page":0,"direction":"ltr","revision":1});
    assert_eq!(
        fixture
            .request("PUT", path, Some(&fixture.manage), input.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    input["reset"] = json!(true);
    let response = fixture
        .request("PUT", path, Some(&fixture.manage), input)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["revision"], 2);
    assert_eq!(fixture.get(path).await["reset_required"], false);
}

fn signature_for(path: &Path) -> String {
    signature(path)
}

#[tokio::test]
async fn symlinks_traversal_and_unregistered_files_never_escape_root() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    fixture.get("/api/v1/library/files/comic/manifest").await;
    let source = fixture.root.join("comic.cbz");
    let outside = fixture.root.parent().unwrap().join("outside.cbz");
    std::fs::rename(&source, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &source).unwrap();
    let response = fixture
        .request(
            "GET",
            "/api/v1/library/files/comic/pages/0",
            Some(&fixture.read),
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        !body(response)
            .await
            .to_string()
            .contains(outside.to_str().unwrap())
    );
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET relative_path='../outside.cbz' WHERE id='comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/manifest",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET root_id=NULL WHERE id='comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/manifest",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn initial_signature_is_verified_before_decoding() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET signature='incorrect' WHERE id='comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/pages/0",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn preserving_size_and_mtime_does_not_hide_in_place_modification() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    fixture.get("/api/v1/library/files/comic/manifest").await;
    let source = fixture.root.join("comic.cbz");
    let before = source.metadata().unwrap();
    let mut bytes = std::fs::read(&source).unwrap();
    bytes[100] ^= 1;
    std::fs::write(&source, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    assert_eq!(mtime(&before), mtime(&source.metadata().unwrap()));
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/pages/0",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn intermediate_directory_symlink_is_checked_on_the_open_descriptor() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let outside = fixture.root.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::rename(fixture.root.join("comic.cbz"), outside.join("comic.cbz")).unwrap();
    std::os::unix::fs::symlink(outside, fixture.root.join("alias")).unwrap();
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET relative_path='alias/comic.cbz' WHERE id='comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        fixture
            .request(
                "GET",
                "/api/v1/library/files/comic/manifest",
                Some(&fixture.read),
                Value::Null
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn pdf_count_and_page_selection_use_the_document_page_tree() {
    let fixture = Fixture::new().await;
    let pdf = fixture._directory.path().join("two-pages.pdf");
    // Original geometric pages, using the same minimal object structure as the
    // project's public-domain single-page PDF fixture.
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>"
            .to_owned(),
        pdf_stream("1 0 0 rg 0 0 100 100 re f\n"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 6 0 R >>"
            .to_owned(),
        pdf_stream("0 0 1 rg 0 0 100 100 re f\n"),
    ];
    let mut document = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(document.len());
        document.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = document.len();
    document.push_str("xref\n0 7\n0000000000 65535 f \n");
    for offset in offsets {
        document.push_str(&format!("{offset:010} 00000 n \n"));
    }
    document.push_str(&format!(
        "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    std::fs::write(&pdf, document).unwrap();
    fixture.file("magazine", pdf.to_str().unwrap(), "pdf").await;
    assert_eq!(
        fixture.get("/api/v1/library/files/magazine/manifest").await["page_count"],
        2
    );
    for (page, expected) in [(0, [255, 0, 0]), (1, [0, 0, 255])] {
        let response = fixture
            .request(
                "GET",
                &format!("/api/v1/library/files/magazine/pages/{page}"),
                Some(&fixture.read),
                Value::Null,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(
            image.get_pixel(image.width() / 2, image.height() / 2).0,
            expected
        );
    }
}

fn pdf_stream(content: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{content}endstream",
        content.len()
    )
}

impl Fixture {
    async fn thumbnail(
        &self,
        id: &str,
        token: Option<&str>,
        etag: Option<&str>,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/library/files/{id}/thumbnail"));
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn page_dimensions(&self, id: &str) -> (u32, u32) {
        let response = self
            .request(
                "GET",
                &format!("/api/v1/library/files/{id}/pages/0"),
                Some(&self.read),
                Value::Null,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let image = image::load_from_memory(&bytes).unwrap();
        (image.width(), image.height())
    }
}

async fn assert_thumbnail(fixture: &Fixture, id: &str, signature: &str) {
    let etag = format!("\"{signature}\"");
    let response = fixture.thumbnail(id, Some(&fixture.read), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=86400"
    );
    assert_eq!(response.headers()[header::ETAG], etag.as_str());
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        image::ImageFormat::Jpeg
    );
    let thumbnail = image::load_from_memory(&bytes).unwrap();
    let (width, height) = fixture.page_dimensions(id).await;
    let expected_width = width.min(240);
    assert_eq!(thumbnail.width(), expected_width);
    let expected_height = (u64::from(height) * u64::from(expected_width) / u64::from(width)).max(1);
    assert_eq!(u64::from(thumbnail.height()), expected_height);

    let cached = fixture.thumbnail(id, Some(&fixture.read), None).await;
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(
        cached.into_body().collect().await.unwrap().to_bytes(),
        bytes
    );

    for validator in [
        etag.clone(),
        format!("W/{etag}"),
        format!("\"other\", {etag}"),
    ] {
        let response = fixture
            .thumbnail(id, Some(&fixture.read), Some(&validator))
            .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{validator}");
        assert_eq!(response.headers()[header::ETAG], etag.as_str());
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=86400"
        );
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
    let mismatch = fixture
        .thumbnail(id, Some(&fixture.read), Some("\"other\""))
        .await;
    assert_eq!(mismatch.status(), StatusCode::OK);
}

#[tokio::test]
async fn cbz_thumbnail_is_bounded_jpeg_with_signature_validator() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    assert_eq!(
        fixture.thumbnail("comic", None, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .thumbnail("comic", None, Some(&format!("\"{signature}\"")))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .thumbnail("missing", Some(&fixture.read), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_thumbnail(&fixture, "comic", &signature).await;
}

#[tokio::test]
async fn pdf_thumbnail_is_bounded_jpeg_with_signature_validator() {
    let fixture = Fixture::new().await;
    let signature = fixture
        .file("magazine", "tests/fixtures/single-page.pdf", "pdf")
        .await;
    assert_thumbnail(&fixture, "magazine", &signature).await;
}

#[tokio::test]
async fn thumbnail_rejects_a_changed_source() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET signature=? WHERE id='comic'")
        .bind("0".repeat(64))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        fixture
            .thumbnail("comic", Some(&fixture.read), None)
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn file_context_lists_covered_units_in_catalog_order() {
    let fixture = Fixture::new().await;
    fixture
        .file("comic", "tests/fixtures/natural-order.cbz", "cbz")
        .await;
    let mut tx = fixture.store.begin_write().await.unwrap();
    for statement in [
        "INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('comic','empty','user_confirmed')",
        "INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('loose','/loose.cbz','cbz','sig',1)",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let path = "/api/v1/library/files/comic/context";
    assert_eq!(
        fixture
            .request("GET", path, None, Value::Null)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let unit = |id: &str, label: &str| {
        json!({
            "publication_id": "publication",
            "publication_title": "Fixture",
            "content_type": "magazine",
            "edition_id": "edition",
            "unit_id": id,
            "unit_label": label,
            "unit_kind": "issue",
        })
    };
    assert_eq!(
        fixture.get(path).await,
        json!({"items": [unit("unit", "1"), unit("empty", "2")]})
    );
    assert_eq!(
        fixture.get("/api/v1/library/files/loose/context").await,
        json!({"items": []})
    );
    let missing = fixture
        .request(
            "GET",
            "/api/v1/library/files/missing/context",
            Some(&fixture.read),
            Value::Null,
        )
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
