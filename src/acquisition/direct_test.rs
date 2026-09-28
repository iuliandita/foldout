use super::*;
use crate::{
    catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
    library::roots::Library,
    settings::{CreateIntegration, EncryptionKey, SecretUpdate},
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use std::sync::{Arc, Mutex};

static TEST_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const ARCHIVE: &[u8] = include_bytes!("../../tests/fixtures/natural-order.cbz");
#[derive(Default)]
struct Seen {
    transfers: usize,
    fail: bool,
    changed: bool,
    metadata_change: u8,
    missing_manifest_page: bool,
    requests: Vec<String>,
}
struct Server {
    address: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(store: SqliteStore, host: &'static str) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let state = (store, seen.clone(), host);
        let app=Router::new().fallback(|State((store,seen,host)):State<(SqliteStore,Arc<Mutex<Seen>>, &'static str)>,request:Request<Body>| async move {
            let path=request.uri().path();
            assert!(!request.headers().contains_key("authorization"));
            assert!(!request.headers().contains_key("cookie"));
            if path=="/owned.cbz" {
                let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM direct_acquisitions WHERE attempted=1 AND state='downloading'").fetch_one(store.reader()).await.unwrap();
                assert_eq!(count,1,"GET requires a committed fence");
                let tx=tokio::time::timeout(Duration::from_secs(2),store.begin_write()).await.expect("network must not hold write transaction").unwrap();
                tx.rollback().await.unwrap();
                let mut state=seen.lock().unwrap();state.transfers+=1;state.requests.push(request.uri().to_string());
                return Response::builder().status(if state.fail {503} else {200}).header("content-type","application/zip").body(Body::from(ARCHIVE)).unwrap();
            }
            let state=seen.lock().unwrap();
            let body=if path=="/" {
                "<div class=post-list-posts><article><h1 class=post-title><a href='/dc/owned-1/'>Owned Fixture</a></h1></article></div>".into()
            } else {
                assert_eq!(path,"/dc/owned-1/");
                let token=if state.changed {"changed-token"} else {"owned-secret-token"};
                format!("<h1 class=post-title>Owned Fixture</h1><article class=post-body><section class=post-contents><a class=aio-red href='https://{host}/owned.cbz?token={token}'>Download</a><a class=aio-blue href='https://pixeldrain.com/u/manual-token'>Mirror</a></section></article>")
            };
            Response::builder().header("content-type","text/html").header("set-cookie","private=cookie").body(Body::from(body)).unwrap()
        }).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            address,
            seen,
            task,
        }
    }
}
struct Fixture {
    _state: tempfile::TempDir,
    source: tempfile::TempDir,
    destination: tempfile::TempDir,
    store: SqliteStore,
    settings: Settings,
    direct: Direct,
    server: Server,
    integration: String,
    post: String,
    request: DirectRequest,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_host("fs3.comicfiles.ru").await
    }
    async fn with_host(host: &'static str) -> Self {
        let state = crate::search::mock::private_directory();
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(state.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(state.path()).await.unwrap(),
        );
        let server = Server::new(store.clone(), host).await;
        let integration = settings
            .create(CreateIntegration {
                kind: IntegrationKind::GetComics,
                label: "Owned Source".into(),
                base_url: format!("http://{}/", server.address),
                enabled: true,
                options: serde_json::json!({}),
                api_key: SecretUpdate::Preserve,
                username: SecretUpdate::Preserve,
                password: SecretUpdate::Preserve,
            })
            .await
            .unwrap()
            .id;
        let library = Library::new(store.clone());
        let download_root = library
            .register_root("download", source.path())
            .await
            .unwrap()
            .id;
        let destination_root = library
            .register_root("library", destination.path())
            .await
            .unwrap()
            .id;
        let catalog = CatalogRepository::new(store.clone());
        let publication = catalog
            .create_publication(NewPublication {
                content_type: ContentType::Comic,
                title: "Owned Fixture".into(),
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
                label: "1".into(),
                kind: UnitKind::Issue,
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
        let mut direct = Direct::new(store.clone());
        direct.fixture = Some(server.address);
        let posts = direct
            .search(
                &settings,
                "owner",
                DirectSearch {
                    integration_id: integration.clone(),
                    query: "Owned".into(),
                    page: 1,
                },
            )
            .await
            .unwrap();
        let post = posts.posts[0].post_handle.clone();
        let detail = direct.detail(&settings, "owner", &post).await.unwrap();
        let request = DirectRequest {
            link_handle: detail.links[0].link_handle.clone(),
            unit_id: unit.id,
            download_root_id: download_root,
            destination: DirectDestination {
                root_id: destination_root,
                relative_path: "owned.cbz".into(),
            },
        };
        Self {
            _state: state,
            source,
            destination,
            store,
            settings,
            direct,
            server,
            integration,
            post,
            request,
        }
    }
    async fn enqueue(&self, key: &str) -> DirectAcquisition {
        self.direct
            .create(&self.settings, "owner", key, self.request.clone())
            .await
            .unwrap()
    }
    async fn tick(&self) {
        assert!(self.direct.tick(self.settings.clone()).await.unwrap());
    }
}

#[tokio::test]
async fn owned_download_is_fenced_validated_copied_and_cataloged_only_after_import() {
    let _serial = TEST_GATE.lock().await;
    for host in ["fs3.comicfiles.ru", "twlv.comicfiles.ru"] {
        let f = Fixture::with_host(host).await;
        let intent = f.enqueue("one").await;
        f.tick().await;
        let downloaded = f.direct.get("owner", &intent.id).await.unwrap();
        assert_eq!(downloaded.state, "downloaded");
        assert_eq!(downloaded.downloaded_bytes, ARCHIVE.len() as i64);
        assert!(!f.destination.path().join("owned.cbz").exists());
        let coverage: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_coverage")
            .fetch_one(f.store.reader())
            .await
            .unwrap();
        assert_eq!(coverage, 0);
        for _ in 0..8 {
            if f.direct.get("owner", &intent.id).await.unwrap().state == "completed" {
                break;
            }
            f.tick().await;
        }
        assert_eq!(
            f.direct.get("owner", &intent.id).await.unwrap().state,
            "completed"
        );
        let saved = f
            .source
            .path()
            .join(format!(".library-downloads/{}/file.cbz", intent.id));
        assert_eq!(std::fs::read(&saved).unwrap(), ARCHIVE);
        assert_eq!(
            std::fs::read(f.destination.path().join("owned.cbz")).unwrap(),
            ARCHIVE
        );
        assert_eq!(
            std::fs::metadata(&saved).unwrap().nlink(),
            1,
            "copy must leave independent source"
        );
        assert_eq!(
            std::fs::metadata(saved.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::metadata(&saved).unwrap().mode() & 0o777, 0o600);
        assert_eq!(f.server.seen.lock().unwrap().transfers, 1);
        let coverage: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM file_coverage WHERE unit_id=?")
                .bind(&f.request.unit_id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
        assert_eq!(coverage, 1);
        assert_eq!(f.enqueue("one").await.id, intent.id);
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
        let rows = sqlx::query("SELECT post_path,link_digest FROM direct_selections")
            .fetch_all(f.store.reader())
            .await
            .unwrap();
        for row in rows {
            for name in ["post_path", "link_digest"] {
                let value: String = row.get(name);
                assert!(!value.contains("token"));
                assert!(!value.contains("https:"));
            }
        }
        let public = serde_json::to_string(&downloaded).unwrap();
        assert!(!public.contains("token"));
        assert!(!public.contains(".library-downloads"));
    }
}

#[tokio::test]
async fn handles_are_stable_user_bound_and_idempotency_survives_configuration_changes() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let again = f
        .direct
        .detail(&f.settings, "owner", &f.post)
        .await
        .unwrap();
    assert_eq!(again.links[0].link_handle, f.request.link_handle);
    assert!(matches!(
        f.direct.detail(&f.settings, "another-owner", &f.post).await,
        Err(DirectError::NotFound)
    ));
    assert!(matches!(
        f.direct
            .resolve(&f.settings, "another-owner", &f.request.link_handle)
            .await,
        Err(DirectError::NotFound)
    ));
    let intent = f.enqueue("stable").await;
    let mut changed = f.request.clone();
    changed.destination.relative_path = "another.cbz".into();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "stable", changed)
            .await,
        Err(DirectError::Conflict)
    ));
    f.settings
        .update(
            &f.integration,
            crate::settings::UpdateIntegration {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(f.enqueue("stable").await.id, intent.id);
    assert!(matches!(
        f.direct.get("another-owner", &intent.id).await,
        Err(DirectError::NotFound)
    ));
    f.tick().await;
    assert_eq!(
        f.direct
            .get("owner", &intent.id)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("configuration_changed")
    );
}

#[tokio::test]
async fn interrupted_and_failed_transfers_never_retry_or_append() {
    let _serial = TEST_GATE.lock().await;
    for interrupted in [true, false] {
        let f = Fixture::new().await;
        let intent = f.enqueue("fence").await;
        if interrupted {
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query(
                "UPDATE direct_acquisitions SET attempted=1,state='downloading' WHERE id=?",
            )
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
        } else {
            f.server.seen.lock().unwrap().fail = true;
        }
        f.tick().await;
        assert_eq!(
            f.direct.get("owner", &intent.id).await.unwrap().state,
            "needs_review"
        );
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
        assert_eq!(
            f.server.seen.lock().unwrap().transfers,
            usize::from(!interrupted)
        );
        assert!(!f.destination.path().join("owned.cbz").exists());
        assert_eq!(f.enqueue("fence").await.id, intent.id);
    }
}

#[tokio::test]
async fn changed_links_manual_hosts_and_existing_destinations_are_not_downloaded() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let detail = f
        .direct
        .detail(&f.settings, "owner", &f.post)
        .await
        .unwrap();
    let mut manual = f.request.clone();
    manual.link_handle = detail.links[1].link_handle.clone();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "manual", manual)
            .await,
        Err(DirectError::ManualAction)
    ));
    let intent = f.enqueue("changed").await;
    f.server.seen.lock().unwrap().changed = true;
    f.tick().await;
    assert_eq!(
        f.direct
            .get("owner", &intent.id)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("source_changed")
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
    let f = Fixture::new().await;
    let intent = f.enqueue("existing").await;
    std::fs::write(
        f.destination.path().join("owned.cbz"),
        b"existing-user-data",
    )
    .unwrap();
    f.tick().await;
    assert_eq!(
        f.direct
            .get("owner", &intent.id)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("local_review")
    );
    assert_eq!(
        std::fs::read(f.destination.path().join("owned.cbz")).unwrap(),
        b"existing-user-data"
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
}

#[tokio::test]
async fn source_replacement_and_private_stage_symlinks_require_review() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let intent = f.enqueue("changed-source").await;
    f.tick().await;
    let saved = f
        .source
        .path()
        .join(format!(".library-downloads/{}/file.cbz", intent.id));
    std::fs::write(&saved, b"changed").unwrap();
    f.tick().await;
    assert_eq!(
        f.direct.get("owner", &intent.id).await.unwrap().state,
        "needs_review"
    );
    assert!(!f.destination.path().join("owned.cbz").exists());
    let f = Fixture::new().await;
    let intent = f.enqueue("symlink").await;
    std::os::unix::fs::symlink(
        f.destination.path(),
        f.source.path().join(".library-downloads"),
    )
    .unwrap();
    f.tick().await;
    assert_eq!(
        f.direct
            .get("owner", &intent.id)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("local_review")
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
}

#[test]
fn public_address_and_exact_host_policy_rejects_local_and_transition_targets() {
    for address in [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::ffff:8.8.8.8",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
        "2002:0808:0808::1",
        "3fff::1",
    ] {
        assert!(!public_ip(address.parse().unwrap()), "{address}");
    }
    for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
        assert!(public_ip(address.parse().unwrap()), "{address}");
    }
    for host in ["fs3.comicfiles.ru", "twlv.comicfiles.ru"] {
        for (extension, expected) in [
            ("cbz", "cbz"),
            ("zip", "cbz"),
            ("cbr", "cbr"),
            ("rar", "cbr"),
        ] {
            let url = reqwest::Url::parse(&format!("https://{host}:443/f.{extension}?token=owned"))
                .unwrap();
            assert_eq!(archive_format(&url).unwrap(), expected);
        }
        for url in [
            format!("http://{host}/f.cbz"),
            format!("https://{host}.evil.invalid/f.cbz"),
            format!("https://sub.{host}/f.cbz"),
            format!("https://evil-{host}/f.cbz"),
            format!("https://{host}:8443/f.cbz"),
            format!("https://user:secret@{host}/f.cbz"),
            format!("https://{host}/f.cbz#secret"),
            format!("https://{host}/f.html"),
            "https://127.0.0.1/f.cbz".into(),
        ] {
            assert!(
                archive_format(&reqwest::Url::parse(&url).unwrap()).is_err(),
                "{url}"
            );
        }
    }
}

#[tokio::test]
async fn stream_rejects_redirects_html_partial_content_and_declared_oversize() {
    for (status, mime, bytes, limit) in [
        (302, "application/zip", b"owned".as_slice(), 10),
        (200, "text/html", b"<html>".as_slice(), 10),
        (206, "application/zip", b"owned".as_slice(), 10),
        (200, "application/zip", b"owned".as_slice(), 4),
    ] {
        let app = Router::new().fallback(move || async move {
            Response::builder()
                .status(status)
                .header("content-type", mime)
                .header("location", "http://127.0.0.1:1/not-requested")
                .body(Body::from(bytes))
                .unwrap()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let file = tempfile::tempfile().unwrap();
        assert!(
            transfer(
                client_builder().build().unwrap(),
                reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
                &file,
                limit
            )
            .await
            .is_err()
        );
        assert_eq!(file.metadata().unwrap().len(), 0);
        task.abort();
    }
}

#[tokio::test]
async fn manage_scope_is_required_for_every_direct_route() {
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    auth.setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let key = auth
        .create_key("read-only", crate::auth::Scope::Read)
        .await
        .unwrap();
    let app = crate::httpapi::direct::routes(crate::httpapi::direct::DirectContext {
        direct: f.direct.clone(),
        settings: f.settings.clone(),
        auth: crate::httpapi::auth::AuthContext {
            service: auth,
            origin: "http://localhost".into(),
        },
    });
    for (method, path, body) in [
        (
            "POST",
            "/api/v1/direct/search",
            serde_json::json!({"integration_id":f.integration,"query":"Owned"}),
        ),
        (
            "POST",
            "/api/v1/direct/chapters",
            serde_json::json!({"integration_id":f.integration,"manga_id":MANGA_ID,"language":"en"}),
        ),
        (
            "POST",
            "/api/v1/direct/details",
            serde_json::json!({"post_handle":f.post}),
        ),
        (
            "POST",
            "/api/v1/direct/resolve",
            serde_json::json!({"link_handle":f.request.link_handle}),
        ),
        (
            "POST",
            "/api/v1/direct/acquisitions",
            serde_json::to_value(&f.request).unwrap(),
        ),
        (
            "POST",
            "/api/v1/direct/acquisitions/not-owned/cancel",
            serde_json::json!({}),
        ),
        ("GET", "/api/v1/direct/acquisitions", serde_json::json!({})),
        (
            "GET",
            "/api/v1/direct/acquisitions/not-owned",
            serde_json::json!({}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {}", key.secret))
                    .header("content-type", "application/json")
                    .header("idempotency-key", "denied")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
}

#[tokio::test]
async fn chunked_stream_is_bounded_without_content_length() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut bytes = [0u8; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            assert!(request.len() < 8192);
        }
        let _=socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n28\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n28\r\nyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\r\n0\r\n\r\n").await;
    });
    let file = tempfile::tempfile().unwrap();
    let result = transfer(
        client_builder().build().unwrap(),
        reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        &file,
        50,
    )
    .await;
    assert!(matches!(result, Err(DirectError::SizeLimit)));
    assert!(file.metadata().unwrap().len() <= 50);
    server.await.unwrap();
}

#[tokio::test]
async fn durable_worker_lock_excludes_other_instances_and_stage_is_exclusive() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let lock = f.direct.worker_lock().await.unwrap().unwrap();
    let other = Direct::new(f.store.clone());
    assert!(other.worker_lock().await.unwrap().is_none());
    drop(lock);
    assert!(other.worker_lock().await.unwrap().is_some());
    let stage_id = Uuid::new_v4().to_string();
    let _stage = Stage::create(f.source.path(), &stage_id).unwrap();
    assert!(matches!(
        Stage::create(f.source.path(), &stage_id),
        Err(DirectError::LocalReview)
    ));
    assert_eq!(
        std::fs::read(
            f.source
                .path()
                .join(format!(".library-downloads/{stage_id}/filepartial"))
        )
        .unwrap(),
        b""
    );
}

#[tokio::test]
async fn ignored_fence_update_prevents_archive_get() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let intent = f.enqueue("ignored-fence").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("CREATE TRIGGER reject_direct_fence BEFORE UPDATE OF attempted ON direct_acquisitions WHEN NEW.attempted=1 BEGIN SELECT RAISE(IGNORE); END")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    f.tick().await;
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
    let result = f.direct.get("owner", &intent.id).await.unwrap();
    assert_eq!(result.state, "needs_review");
    assert!(!result.attempted);
}

#[tokio::test]
async fn recovery_hash_rejects_oversized_sparse_files_before_reading() {
    let file = tempfile::tempfile().unwrap();
    file.set_len(MAX_BYTES + 1).unwrap();
    assert!(matches!(
        fingerprint_file(&file).await,
        Err(DirectError::SizeLimit)
    ));
}

const MANGA_ID: &str = "11111111-1111-4111-8111-111111111111";
const CHAPTER_ID: &str = "22222222-2222-4222-8222-222222222222";
const GROUP_ID: &str = "33333333-3333-4333-8333-333333333333";

impl Server {
    async fn manga(store: SqliteStore) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let app = Router::new().fallback(|State((store, seen)): State<(SqliteStore, Arc<Mutex<Seen>>)>, request: Request<Body>| async move {
            assert!(!request.headers().contains_key("authorization"));
            assert!(!request.headers().contains_key("cookie"));
            let path = request.uri().path();
            if path.starts_with("/data/") {
                let row = sqlx::query("SELECT attempted,state,manifest_identity,expected_pages FROM direct_acquisitions").fetch_one(store.reader()).await.unwrap();
                assert!(row.get::<bool,_>("attempted"));
                assert_eq!(row.get::<String,_>("state"), "downloading");
                assert_eq!(row.get::<String,_>("manifest_identity").len(), 64);
                assert_eq!(row.get::<i64,_>("expected_pages"), 2);
                let tx = tokio::time::timeout(Duration::from_secs(2), store.begin_write()).await.unwrap().unwrap();
                tx.rollback().await.unwrap();
                let fail = {
                    let mut state = seen.lock().unwrap();
                    state.transfers += 1;
                    state.requests.push(path.to_owned());
                    state.fail && path.ends_with("second.png")
                };
                let mut png = std::io::Cursor::new(Vec::new());
                image::DynamicImage::new_rgb8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
                return Response::builder().status(if fail { 404 } else { 200 }).header("content-type", "image/png").body(Body::from(png.into_inner())).unwrap();
            }
            let state = seen.lock().unwrap();
            let change = state.metadata_change;
            let chapter = serde_json::json!({
                "id":CHAPTER_ID,"type":"chapter",
                "attributes":{"translatedLanguage":if change==2 {"fr"} else {"en"},"chapter":"1.5","volume":null,"title":"Owned chapter","pages":2,"version":if change==1 {2} else {1},"externalUrl":null,"isUnavailable":false},
                "relationships":[{"id":if change==3 {GROUP_ID} else {MANGA_ID},"type":"manga"},{"id":if change==4 {MANGA_ID} else {GROUP_ID},"type":"scanlation_group","attributes":{"name":"Owned group"}}]
            });
            let body = if path == "/chapter" {
                serde_json::json!({"result":"ok","response":"collection","data":[chapter],"limit":20,"offset":0,"total":1})
            } else if path == format!("/chapter/{CHAPTER_ID}") {
                serde_json::json!({"result":"ok","response":"entity","data":chapter})
            } else {
                assert_eq!(path, format!("/at-home/server/{CHAPTER_ID}"));
                let files = if state.missing_manifest_page { vec!["first.png"] } else { vec!["first.png","second.png"] };
                serde_json::json!({"result":"ok","baseUrl":"https://uploads.mangadex.org","chapter":{"hash":"0123456789abcdef0123456789abcdef","data":files,"dataSaver":files}})
            };
            Response::builder().header("content-type","application/json").body(Body::from(body.to_string())).unwrap()
        }).with_state((store, seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            address,
            seen,
            task,
        }
    }
}
impl Fixture {
    async fn manga() -> Self {
        let mut f = Self::new().await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE publications SET content_type='manga'")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE units SET kind='chapter'")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        f.server = Server::manga(f.store.clone()).await;
        f.integration = f
            .settings
            .create(CreateIntegration {
                kind: IntegrationKind::MangaDex,
                label: "Owned chapters".into(),
                base_url: format!("http://{}/", f.server.address),
                enabled: true,
                options: serde_json::json!({}),
                api_key: SecretUpdate::Preserve,
                username: SecretUpdate::Preserve,
                password: SecretUpdate::Preserve,
            })
            .await
            .unwrap()
            .id;
        f.direct.fixture = Some(f.server.address);
        let page = f.chapter_page().await;
        f.request.link_handle = page.chapters[0].link_handle.clone();
        f
    }
    async fn chapter_page(&self) -> DirectChapterPage {
        self.direct
            .chapters(
                &self.settings,
                "owner",
                DirectChapterSearch {
                    integration_id: self.integration.clone(),
                    manga_id: MANGA_ID.into(),
                    language: "en".into(),
                    page: 1,
                    limit: 20,
                },
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn manga_complete_chapter_uses_same_fence_and_copy_journal() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::manga().await;
    let page = f.chapter_page().await;
    assert_eq!(page.chapters[0].link_handle, f.request.link_handle);
    let public = serde_json::to_string(&page).unwrap();
    assert!(!public.contains("https:"));
    assert!(!public.contains("external_url"));
    let intent = f.enqueue("manga-complete").await;
    f.tick().await;
    assert_eq!(
        f.direct.get("owner", &intent.id).await.unwrap().state,
        "downloaded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM file_coverage")
            .fetch_one(f.store.reader())
            .await
            .unwrap(),
        0
    );
    let saved = f
        .source
        .path()
        .join(format!(".library-downloads/{}/file.cbz", intent.id));
    let manifest = crate::reader::archive::ArchiveDecoder::new()
        .manifest(&saved)
        .await
        .unwrap();
    assert_eq!(manifest.pages.len(), 2);
    assert!(manifest.pages[0].name < manifest.pages[1].name);
    for _ in 0..10 {
        if f.direct.get("owner", &intent.id).await.unwrap().state == "completed" {
            break;
        }
        f.tick().await;
    }
    assert_eq!(
        f.direct.get("owner", &intent.id).await.unwrap().state,
        "completed"
    );
    assert_eq!(
        std::fs::read(&saved).unwrap(),
        std::fs::read(f.destination.path().join("owned.cbz")).unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM file_coverage WHERE unit_id=?")
            .bind(&f.request.unit_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap(),
        1
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 2);
    assert_eq!(f.enqueue("manga-complete").await.id, intent.id);
    let mut tx = f.store.begin_write().await.unwrap();
    assert!(
        sqlx::query("UPDATE direct_acquisitions SET expected_pages=1 WHERE id=?")
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn manga_metadata_identity_changes_never_cross_transfer_fence() {
    let _serial = TEST_GATE.lock().await;
    for change in 1..=4 {
        let f = Fixture::manga().await;
        let intent = f.enqueue("metadata").await;
        f.server.seen.lock().unwrap().metadata_change = change;
        f.tick().await;
        let result = f.direct.get("owner", &intent.id).await.unwrap();
        assert_eq!(result.state, "needs_review");
        assert_eq!(result.reason.as_deref(), Some("source_changed"));
        assert!(!result.attempted);
        assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
    }
}

#[tokio::test]
async fn manga_missing_pages_never_publish_and_attempted_transfers_never_resume() {
    let _serial = TEST_GATE.lock().await;
    for missing_manifest in [true, false] {
        let f = Fixture::manga().await;
        let intent = f.enqueue("missing").await;
        {
            let mut state = f.server.seen.lock().unwrap();
            state.missing_manifest_page = missing_manifest;
            state.fail = !missing_manifest;
        }
        f.tick().await;
        let result = f.direct.get("owner", &intent.id).await.unwrap();
        assert_eq!(result.state, "needs_review");
        assert_eq!(result.attempted, !missing_manifest);
        assert!(!f.destination.path().join("owned.cbz").exists());
        assert!(
            !f.source
                .path()
                .join(format!(".library-downloads/{}/file.cbz", intent.id))
                .exists()
        );
        let transfers = f.server.seen.lock().unwrap().transfers;
        assert_eq!(transfers, if missing_manifest { 0 } else { 2 });
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
        assert_eq!(f.server.seen.lock().unwrap().transfers, transfers);
        assert_eq!(f.enqueue("missing").await.id, intent.id);
    }
}

#[tokio::test]
async fn direct_rejects_wrong_content_type_language_and_foreign_chapter_handles() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE publications SET content_type='manga'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "wrong-kind", f.request.clone())
            .await,
        Err(DirectError::Invalid)
    ));
    let f = Fixture::manga().await;
    assert!(matches!(
        f.direct
            .create(&f.settings, "other", "foreign", f.request.clone())
            .await,
        Err(DirectError::NotFound)
    ));
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE editions SET language='fr'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "wrong-language", f.request.clone())
            .await,
        Err(DirectError::Invalid)
    ));
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE editions SET language='en'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE publications SET content_type='comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "wrong-kind", f.request.clone())
            .await,
        Err(DirectError::Invalid)
    ));
}

#[tokio::test]
async fn cancellation_releases_reservation_but_preserves_files_evidence_and_replay() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::manga().await;
    let intent = f.enqueue("cancel-original").await;
    let stage = Stage::create(f.source.path(), &intent.id).unwrap();
    use std::io::Write;
    (&stage.file).write_all(b"owned partial evidence").unwrap();
    let before = identity(&stage.file).unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE direct_acquisitions SET state='needs_review',reason='interrupted_transfer',attempted=1,manifest_identity=?,expected_pages=2 WHERE id=?")
        .bind("a".repeat(64)).bind(&intent.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    std::fs::write(
        f.destination.path().join("owned.cbz"),
        b"existing destination",
    )
    .unwrap();
    let canceled = f.direct.cancel("owner", &intent.id).await.unwrap();
    assert_eq!(canceled.state, "canceled");
    assert!(canceled.canceled_at.is_some());
    assert!(canceled.attempted);
    assert_eq!(canceled.reason.as_deref(), Some("interrupted_transfer"));
    assert_eq!(identity(&stage.file).unwrap(), before);
    assert_eq!(
        std::fs::read(
            f.source
                .path()
                .join(format!(".library-downloads/{}/filepartial", intent.id))
        )
        .unwrap(),
        b"owned partial evidence"
    );
    assert_eq!(
        std::fs::read(f.destination.path().join("owned.cbz")).unwrap(),
        b"existing destination"
    );
    assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
    assert_eq!(f.enqueue("cancel-original").await.id, intent.id);
    let replacement = f.enqueue("cancel-replacement").await;
    assert_ne!(replacement.id, intent.id);
    assert_eq!(
        f.direct
            .cancel("owner", &intent.id)
            .await
            .unwrap()
            .canceled_at,
        canceled.canceled_at
    );
    assert_eq!(
        f.direct.get("owner", &replacement.id).await.unwrap().state,
        "queued"
    );
    let mut changed = f.request.clone();
    changed.destination.relative_path = "different.cbz".into();
    assert!(matches!(
        f.direct
            .create(&f.settings, "owner", "cancel-original", changed)
            .await,
        Err(DirectError::Conflict)
    ));
    let mut tx = f.store.begin_write().await.unwrap();
    assert!(
        sqlx::query("UPDATE direct_acquisitions SET state='queued',canceled_at=NULL WHERE id=?")
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    f.tick().await;
    assert_eq!(
        f.direct.get("owner", &replacement.id).await.unwrap().state,
        "needs_review"
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
}

#[tokio::test]
async fn cancellation_checks_owner_worker_locks_and_states_without_network() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let intent = f.enqueue("cancel-lock").await;
    assert!(matches!(
        f.direct.cancel("other", &intent.id).await,
        Err(DirectError::NotFound)
    ));
    let slot = SLOT
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    assert!(matches!(
        f.direct.cancel("owner", &intent.id).await,
        Err(DirectError::Busy)
    ));
    drop(slot);
    let lock = f.direct.worker_lock().await.unwrap().unwrap();
    assert!(matches!(
        f.direct.cancel("owner", &intent.id).await,
        Err(DirectError::Busy)
    ));
    drop(lock);
    for state in ["downloading", "importing", "completed"] {
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE direct_acquisitions SET state=? WHERE id=?")
            .bind(state)
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            f.direct.cancel("owner", &intent.id).await,
            Err(DirectError::ReviewRequired)
        ));
    }
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE direct_acquisitions SET state='queued' WHERE id=?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Registered download directory accessibility is irrelevant to metadata-only cancellation.
    let moved = f.source.path().with_extension("moved");
    std::fs::rename(f.source.path(), &moved).unwrap();
    let outcome = f.direct.cancel("owner", &intent.id).await;
    std::fs::rename(&moved, f.source.path()).unwrap();
    assert_eq!(outcome.unwrap().state, "canceled");
    assert_eq!(f.server.seen.lock().unwrap().transfers, 0);
}

#[tokio::test]
async fn cancellation_rejects_every_import_phase_even_for_idle_direct_intents() {
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let intent = f.enqueue("cancel-journal").await;
    f.tick().await;
    let service = ImportService::new(f.store.clone());
    service
        .plan(
            &intent.import_id,
            InternalImportRequest {
                source_root: f.request.download_root_id.clone(),
                source_relative: format!(".library-downloads/{}/file.cbz", intent.id),
                destination_root: f.request.destination.root_id.clone(),
                destination_relative: f.request.destination.relative_path.clone(),
                unit_id: f.request.unit_id.clone(),
                policy: ImportPolicy::Copy,
            },
        )
        .await
        .unwrap();
    for phase in [
        "planned",
        "staged",
        "verified",
        "finalized",
        "cataloged",
        "cleanup_pending",
        "done",
    ] {
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE import_operations SET phase=? WHERE id=?")
            .bind(phase)
            .bind(&intent.import_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(
            matches!(
                f.direct.cancel("owner", &intent.id).await,
                Err(DirectError::ReviewRequired)
            ),
            "{phase}"
        );
        assert_eq!(
            f.direct.get("owner", &intent.id).await.unwrap().state,
            "downloaded"
        );
    }
    assert_eq!(f.server.seen.lock().unwrap().transfers, 1);
}

#[tokio::test]
async fn cancellation_migration_preserves_evidence_and_cross_pipeline_reservations() {
    use sqlx::Connection;
    let mut db = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE integrations(id TEXT PRIMARY KEY); CREATE TABLE units(id TEXT PRIMARY KEY); CREATE TABLE library_roots(id TEXT PRIMARY KEY); CREATE TABLE acquisition_runs(id TEXT PRIMARY KEY,unit_id TEXT,state TEXT,destination_root TEXT,destination_relative TEXT); INSERT INTO integrations VALUES('i'); INSERT INTO units VALUES('u'); INSERT INTO library_roots VALUES('r');")
        .execute(&mut db).await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/010_direct.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/011_mangadex_direct.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO direct_selections VALUES('h','o','i','fingerprint','/dc/owned/','digest','getcomics'); INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,downloaded_bytes,content_digest,source_identity,root_identity,destination_identity,source_relative,created_at,updated_at,manifest_identity,expected_pages) VALUES('a','o','key','request','h','u','r','r','owned.cbz','journal','needs_review','interrupted_transfer',1,12,'content','source','root','destination','partial',10,20,NULL,NULL);")
        .execute(&mut db).await.unwrap();
    let before: String = sqlx::query_scalar("SELECT json_array(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,downloaded_bytes,content_digest,source_identity,root_identity,destination_identity,source_relative,created_at,updated_at,manifest_identity,expected_pages) FROM direct_acquisitions")
        .fetch_one(&mut db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/013_direct_resolution.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let after: String = sqlx::query_scalar("SELECT json_array(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,downloaded_bytes,content_digest,source_identity,root_identity,destination_identity,source_relative,created_at,updated_at,manifest_identity,expected_pages) FROM direct_acquisitions")
        .fetch_one(&mut db).await.unwrap();
    assert_eq!(before, after);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        sqlx::query("INSERT INTO acquisition_runs VALUES('indexer','u','queued','r','owned.cbz')")
            .execute(&mut db)
            .await
            .is_err()
    );
    sqlx::query(
        "INSERT INTO acquisition_runs VALUES('association','different','queued',NULL,NULL)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    assert!(sqlx::query("UPDATE acquisition_runs SET destination_root='r',destination_relative='owned.cbz' WHERE id='association'").execute(&mut db).await.is_err());
    sqlx::query("UPDATE direct_acquisitions SET state='canceled',canceled_at=30 WHERE id='a'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO acquisition_runs VALUES('indexer','u','queued','r','owned.cbz')")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE acquisition_runs SET destination_root='r',destination_relative='owned.cbz' WHERE id='association'").execute(&mut db).await.unwrap();
    let insert = "INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state) VALUES(?,'o',?,'request','h','u','r','r','owned.cbz',?,'queued')";
    assert!(
        sqlx::query(insert)
            .bind("b")
            .bind("new-key")
            .bind("new-journal")
            .execute(&mut db)
            .await
            .is_err()
    );
    sqlx::query("UPDATE acquisition_runs SET state='canceled'")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind("b")
            .bind("key")
            .bind("new-journal")
            .execute(&mut db)
            .await
            .is_err()
    );
    sqlx::query(insert)
        .bind("b")
        .bind("new-key")
        .bind("new-journal")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind("c")
            .bind("third-key")
            .bind("third-journal")
            .execute(&mut db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE direct_acquisitions SET attempted=0 WHERE id='a'")
            .execute(&mut db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancel_http_is_bounded_owner_authorized_and_idempotent() {
    use tower::ServiceExt;
    let _serial = TEST_GATE.lock().await;
    let f = Fixture::new().await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    auth.setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let key = auth
        .create_key("manage", crate::auth::Scope::Manage)
        .await
        .unwrap();
    // Obtain the actual principal using the same authorization path as the handler.
    let context = crate::httpapi::auth::AuthContext {
        service: auth,
        origin: "http://localhost".into(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", key.secret).parse().unwrap(),
    );
    let principal = crate::httpapi::auth::authorize(
        &context,
        &headers,
        &axum::http::Method::POST,
        crate::auth::Scope::Manage,
    )
    .await
    .unwrap();
    let intent = f.enqueue("http-cancel").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE direct_acquisitions SET owner=? WHERE id=?")
        .bind(&principal.user_id)
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let app = crate::httpapi::direct::routes(crate::httpapi::direct::DirectContext {
        direct: f.direct.clone(),
        settings: f.settings.clone(),
        auth: context,
    });
    for (body, status) in [
        ("{\"retry\":true}", StatusCode::BAD_REQUEST),
        ("{}", StatusCode::OK),
        ("{}", StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/direct/acquisitions/{}/cancel", intent.id))
                    .header("authorization", format!("Bearer {}", key.secret))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
    }
}
