use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    Resume,
    ChangedEtag,
    ChangedDate,
    NoValidators,
    WeakEtag,
    NoRanges,
    IgnoreRange,
    WrongStart,
    ChangedLength,
    DateOnly,
}
struct RangeFixture {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for RangeFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl RangeFixture {
    async fn new(store: SqliteStore, behavior: Behavior) -> Self {
        Self::with_pages(store, behavior, false).await
    }
    async fn with_pages(store: SqliteStore, behavior: Behavior, manga: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            let archive = if manga {
                let mut png = std::io::Cursor::new(Vec::new());
                image::DynamicImage::new_rgb8(2, 2)
                    .write_to(&mut png, image::ImageFormat::Png)
                    .unwrap();
                png.into_inner()
            } else {
                ARCHIVE.to_vec()
            };
            let bytes = archive.as_slice();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("accept-encoding: identity")
                );
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                assert!(!request.to_ascii_lowercase().contains("cookie:"));
                let is_resume = request.to_ascii_lowercase().contains("range: bytes=");
                let count:i64 = sqlx::query_scalar("SELECT COUNT(*) FROM direct_acquisitions WHERE state='downloading' AND attempted=1").fetch_one(store.reader()).await.unwrap();
                assert_eq!(count, 1);
                let tx = tokio::time::timeout(Duration::from_secs(2), store.begin_write())
                    .await
                    .expect("GET must not hold writer")
                    .unwrap();
                tx.rollback().await.unwrap();
                seen.lock().unwrap().push(request.clone());
                let cut = bytes.len() / 2;
                let etag = match behavior {
                    Behavior::NoValidators | Behavior::DateOnly => "",
                    Behavior::WeakEtag => "ETag: W/\"owned\"\r\n",
                    Behavior::ChangedEtag if is_resume => "ETag: \"changed\"\r\n",
                    _ => "ETag: \"owned\"\r\n",
                };
                let modified = match behavior {
                    Behavior::NoValidators | Behavior::WeakEtag => "",
                    Behavior::ChangedDate if is_resume => {
                        "Last-Modified: Thu, 22 Oct 2026 10:00:00 GMT\r\n"
                    }
                    _ => "Last-Modified: Wed, 21 Oct 2026 10:00:00 GMT\r\n",
                };
                let ranges = if behavior == Behavior::NoRanges {
                    ""
                } else {
                    "Accept-Ranges: bytes\r\n"
                };
                let (status, length, content_range, body) =
                    if is_resume && behavior != Behavior::IgnoreRange {
                        assert!(
                            request
                                .to_ascii_lowercase()
                                .contains(&format!("range: bytes={cut}-"))
                        );
                        assert!(request.contains(if behavior == Behavior::DateOnly {
                            "if-range: Wed, 21 Oct 2026 10:00:00 GMT"
                        } else {
                            "if-range: \"owned\""
                        }));
                        let start = if behavior == Behavior::WrongStart {
                            cut + 1
                        } else {
                            cut
                        };
                        let total = bytes.len() + usize::from(behavior == Behavior::ChangedLength);
                        (
                            206,
                            bytes.len() - cut,
                            format!("Content-Range: bytes {start}-{}/{total}\r\n", total - 1),
                            &bytes[cut..],
                        )
                    } else {
                        (
                            200,
                            bytes.len(),
                            String::new(),
                            if is_resume || (manga && request.contains("first.png")) {
                                bytes
                            } else {
                                &bytes[..cut]
                            },
                        )
                    };
                let headers = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {}\r\nContent-Length: {length}\r\n{etag}{modified}{ranges}{content_range}Connection: close\r\n\r\n",
                    if manga {
                        "image/png"
                    } else {
                        "application/zip"
                    }
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        Self {
            address,
            requests,
            task,
        }
    }
}
fn action(review: &DirectReview) -> TransferAction {
    TransferAction {
        revision: review.revision,
        transfer_fingerprint: review.transfer_fingerprint.clone(),
    }
}

#[tokio::test]
async fn reviewed_transfer_resume_survives_reopen_and_preserves_exact_bytes_and_replay() {
    let _serial = TEST_GATE.lock().await;
    for behavior in [Behavior::Resume, Behavior::DateOnly] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), behavior).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("resume").await;
        f.tick().await;
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        assert_eq!(review.acquisition.state, "needs_review");
        assert!(review.resume.eligible, "{review:?}");
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
        let offset: String = sqlx::query_scalar(
            "SELECT checkpoint FROM direct_transfer_parts WHERE acquisition_id=?",
        )
        .bind(&intent.id)
        .fetch_one(f.store.reader())
        .await
        .unwrap();
        let checkpoint: crate::acquisition::transfer_checkpoint::Checkpoint =
            serde_json::from_str(&offset).unwrap();
        assert_eq!(checkpoint.offset, (ARCHIVE.len() / 2) as u64);
        // Reopen the database and discover the committed interrupted attempt, as after process death.
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE direct_acquisitions SET state='downloading' WHERE id=?")
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let reopened = SqliteStore::open(f._state.path()).await.unwrap();
        let mut direct = Direct::new(reopened);
        direct.fixture = Some(server.address);
        assert!(direct.tick(f.settings.clone()).await.unwrap());
        let fresh = direct.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            direct
                .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await,
            Err(DirectError::ReviewConflict)
        ));
        assert!(matches!(
            direct
                .resume_transfer(&f.settings, "other", &intent.id, action(&fresh))
                .await,
            Err(DirectError::NotFound)
        ));
        let resumed = direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&fresh))
            .await
            .unwrap();
        assert_eq!(resumed.state, "downloaded");
        assert!(resumed.attempted);
        assert_eq!(
            std::fs::read_dir(f.source.path()).unwrap().count(),
            1,
            "only the existing staging area may be written"
        );
        assert!(!f.destination.path().join("owned.cbz").exists());
        assert_eq!(
            std::fs::read(
                f.source
                    .path()
                    .join(format!(".library-downloads/{}/file.cbz", intent.id))
            )
            .unwrap(),
            ARCHIVE
        );
        assert!(matches!(
            direct
                .resume_transfer(&f.settings, "owner", &intent.id, action(&fresh))
                .await,
            Err(DirectError::ReviewConflict)
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(f.enqueue("resume").await.id, intent.id);
        let events:i64=sqlx::query_scalar("SELECT COUNT(*) FROM direct_transfer_events WHERE acquisition_id=? AND action='resume'").bind(&intent.id).fetch_one(f.store.reader()).await.unwrap();
        assert_eq!(events, 1);
        for _ in 0..8 {
            if direct.get("owner", &intent.id).await.unwrap().state == "completed" {
                break;
            }
            assert!(direct.tick(f.settings.clone()).await.unwrap());
        }
        assert_eq!(
            direct.get("owner", &intent.id).await.unwrap().state,
            "completed"
        );
        assert_eq!(
            std::fs::read(f.destination.path().join("owned.cbz")).unwrap(),
            ARCHIVE
        );
    }
}

#[tokio::test]
async fn reviewed_resume_refuses_changed_or_missing_response_evidence_before_append() {
    let _serial = TEST_GATE.lock().await;
    for behavior in [
        Behavior::ChangedEtag,
        Behavior::ChangedDate,
        Behavior::NoValidators,
        Behavior::WeakEtag,
        Behavior::NoRanges,
        Behavior::IgnoreRange,
        Behavior::WrongStart,
        Behavior::ChangedLength,
    ] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), behavior).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("refuse").await;
        f.tick().await;
        let partial = f
            .source
            .path()
            .join(format!(".library-downloads/{}/filepartial", intent.id));
        let original = std::fs::read(&partial).unwrap();
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        let unavailable = matches!(
            behavior,
            Behavior::NoValidators | Behavior::WeakEtag | Behavior::NoRanges
        );
        assert_eq!(review.resume.eligible, !unavailable, "{behavior:?}");
        let error = f
            .direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await
            .unwrap_err();
        assert!(
            if unavailable {
                matches!(error, DirectError::ResumeUnavailable)
            } else {
                matches!(error, DirectError::RestartRequired)
            },
            "{behavior:?}: {error:?}"
        );
        assert_eq!(std::fs::read(&partial).unwrap(), original);
        assert_eq!(
            server.requests.lock().unwrap().len(),
            if unavailable { 1 } else { 2 }
        );
        assert!(!f.destination.path().join("owned.cbz").exists());
        assert!(!f.direct.tick(f.settings.clone()).await.unwrap());
    }
}

#[tokio::test]
async fn tampered_truncated_or_changed_origin_partials_require_restart() {
    let _serial = TEST_GATE.lock().await;
    for change in ["tamper", "truncate", "origin", "extra_bytes"] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), Behavior::Resume).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("tamper").await;
        f.tick().await;
        let partial = f
            .source
            .path()
            .join(format!(".library-downloads/{}/filepartial", intent.id));
        match change {
            "tamper" => {
                let mut bytes = std::fs::read(&partial).unwrap();
                bytes[0] ^= 1;
                std::fs::write(&partial, bytes).unwrap();
            }
            "truncate" => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&partial)
                    .unwrap()
                    .set_len(1)
                    .unwrap();
            }
            "extra_bytes" => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&partial)
                    .unwrap()
                    .write_all(b"unsynced-tail")
                    .unwrap();
            }
            _ => {
                f.direct.fixture = Some("127.0.0.1:1".parse().unwrap());
            }
        }
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            f.direct
                .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await,
            Err(DirectError::RestartRequired)
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn reviewed_restart_retires_the_fence_discards_only_partials_and_creates_once() {
    let _serial = TEST_GATE.lock().await;
    let mut f = Fixture::new().await;
    let server = RangeFixture::new(f.store.clone(), Behavior::NoValidators).await;
    f.direct.fixture = Some(server.address);
    let intent = f.enqueue("restart").await;
    f.tick().await;
    let staging = f
        .source
        .path()
        .join(format!(".library-downloads/{}", intent.id));
    std::fs::write(staging.join("keep.cbz"), b"published evidence").unwrap();
    let review = f.direct.review("owner", &intent.id).await.unwrap();
    assert!(review.restart.eligible);
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "other", &intent.id, action(&review))
            .await,
        Err(DirectError::NotFound)
    ));
    let next = f
        .direct
        .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
        .await
        .unwrap();
    assert_ne!(next.id, intent.id);
    assert_eq!(next.state, "queued");
    assert!(!next.attempted);
    let old = f.direct.get("owner", &intent.id).await.unwrap();
    assert_eq!(old.state, "canceled");
    assert!(old.attempted);
    assert!(!staging.join("filepartial").exists());
    assert_eq!(
        std::fs::read(staging.join("keep.cbz")).unwrap(),
        b"published evidence"
    );
    assert_eq!(f.enqueue("restart").await.id, intent.id);
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::ReviewConflict)
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    // Return to the original fixture for a fresh complete GET from zero.
    f.direct.fixture = Some(f.server.address);
    f.tick().await;
    assert_eq!(
        f.direct.get("owner", &next.id).await.unwrap().state,
        "downloaded"
    );
    assert_eq!(f.server.seen.lock().unwrap().transfers, 1);
    assert!(!f.destination.path().join("owned.cbz").exists());
}

#[tokio::test]
async fn manga_reviewed_resume_requires_exact_manifest_and_complete_page_evidence() {
    let _serial = TEST_GATE.lock().await;
    for change in [
        "none",
        "manifest",
        "missing_row",
        "extra_row",
        "missing_file",
        "extra_file",
        "page_hash",
    ] {
        let mut f = Fixture::manga().await;
        let server = RangeFixture::with_pages(f.store.clone(), Behavior::Resume, true).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("manga-resume").await;
        f.tick().await;
        let initial = f.direct.review("owner", &intent.id).await.unwrap();
        assert!(initial.resume.eligible, "{initial:?}");
        let staging = f
            .source
            .path()
            .join(format!(".library-downloads/{}", intent.id));
        match change {
            "manifest" => {
                f.server.seen.lock().unwrap().changed = true;
            }
            "missing_file" => {
                std::fs::remove_file(staging.join("page-000001.partial")).unwrap();
            }
            "extra_file" => {
                std::fs::write(staging.join("page-000003.partial"), b"extra").unwrap();
            }
            "missing_row" | "extra_row" | "page_hash" => {
                let mut tx = f.store.begin_write().await.unwrap();
                let query = match change {
                    "missing_row" => {
                        "DELETE FROM direct_transfer_parts WHERE acquisition_id=? AND ordinal=1"
                    }
                    "extra_row" => {
                        "INSERT INTO direct_transfer_parts(acquisition_id,ordinal,page_identity) SELECT acquisition_id,3,page_identity FROM direct_transfer_parts WHERE acquisition_id=? AND ordinal=1"
                    }
                    _ => {
                        "UPDATE direct_transfer_parts SET page_identity=printf('%064d',0) WHERE acquisition_id=? AND ordinal=1"
                    }
                };
                sqlx::query(query)
                    .bind(&intent.id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
            _ => {}
        }
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        let result = f
            .direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await;
        if change == "none" {
            assert_eq!(result.unwrap().state, "downloaded");
            assert_eq!(
                server.requests.lock().unwrap().len(),
                3,
                "complete first page must not GET again"
            );
            assert_eq!(
                f.direct.get("owner", &intent.id).await.unwrap().state,
                "downloaded"
            );
            let original_manifest: String =
                sqlx::query_scalar("SELECT manifest_identity FROM direct_acquisitions WHERE id=?")
                    .bind(&intent.id)
                    .fetch_one(f.store.reader())
                    .await
                    .unwrap();
            assert_eq!(original_manifest.len(), 64);
        } else {
            assert!(
                matches!(
                    result,
                    Err(DirectError::RestartRequired | DirectError::LocalReview)
                ),
                "{change}: {result:?}"
            );
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            assert!(!f.destination.path().join("owned.cbz").exists());
            if change == "manifest" {
                let original: String = sqlx::query_scalar(
                    "SELECT manifest_identity FROM direct_acquisitions WHERE id=?",
                )
                .bind(&intent.id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
                let review = f.direct.review("owner", &intent.id).await.unwrap();
                assert!(!review.resume.eligible);
                assert!(review.restart.eligible);
                let restarted = f
                    .direct
                    .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
                    .await
                    .unwrap();
                assert_eq!(
                    server.requests.lock().unwrap().len(),
                    2,
                    "restart only queues the new intent"
                );
                f.tick().await;
                let retained: String = sqlx::query_scalar(
                    "SELECT manifest_identity FROM direct_acquisitions WHERE id=?",
                )
                .bind(&intent.id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
                let fresh: String = sqlx::query_scalar(
                    "SELECT manifest_identity FROM direct_acquisitions WHERE id=?",
                )
                .bind(&restarted.id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
                assert_eq!(retained, original);
                assert_ne!(fresh, original);
            }
        }
    }
}

#[tokio::test]
async fn direct_review_actions_are_scoped_bounded_and_report_distinct_http_codes() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let _serial = TEST_GATE.lock().await;
    let mut f = Fixture::new().await;
    let server = RangeFixture::new(f.store.clone(), Behavior::Resume).await;
    f.direct.fixture = Some(server.address);
    let auth = crate::auth::AuthService::new(f.store.clone());
    auth.setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let key = auth
        .create_key("manage", crate::auth::Scope::Manage)
        .await
        .unwrap();
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
    let handle = Uuid::new_v4().to_string();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path,link_digest,source_kind) SELECT ?,?,integration_id,source_fingerprint,post_path,link_digest,source_kind FROM direct_selections WHERE handle=?")
        .bind(&handle).bind(&principal.user_id).bind(&f.request.link_handle).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    f.request.link_handle = handle;
    let intent = f
        .direct
        .create(
            &f.settings,
            &principal.user_id,
            "http-review",
            f.request.clone(),
        )
        .await
        .unwrap();
    f.tick().await;
    let app = crate::httpapi::direct::routes(crate::httpapi::direct::DirectContext {
        direct: f.direct.clone(),
        settings: f.settings.clone(),
        auth: context,
    });
    let review_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/direct/acquisitions/{}/review", intent.id))
                .header("authorization", format!("Bearer {}", key.secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(review_response.status(), StatusCode::OK);
    assert_eq!(review_response.headers()["cache-control"], "no-store");
    let review: serde_json::Value = serde_json::from_slice(
        &review_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(review["resume"]["eligible"], true);
    let request = serde_json::json!({"revision":review["revision"],"transfer_fingerprint":review["transfer_fingerprint"]});
    for (path, body, status, code) in [
        (
            format!("{}/resume", intent.id),
            serde_json::json!({"revision":0,"transfer_fingerprint":"0".repeat(64)}),
            StatusCode::CONFLICT,
            Some("direct_review_conflict"),
        ),
        (
            format!("{}/resume", intent.id),
            serde_json::json!({"revision":review["revision"],"transfer_fingerprint":review["transfer_fingerprint"],"url":"not-accepted"}),
            StatusCode::BAD_REQUEST,
            Some("invalid_request"),
        ),
        (
            format!("{}/resume", Uuid::new_v4()),
            request.clone(),
            StatusCode::NOT_FOUND,
            Some("not_found"),
        ),
        (
            format!("{}/resume", intent.id),
            request.clone(),
            StatusCode::ACCEPTED,
            None,
        ),
        (
            format!("{}/resume", intent.id),
            request,
            StatusCode::CONFLICT,
            Some("direct_review_conflict"),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/direct/acquisitions/{path}"))
                    .header("authorization", format!("Bearer {}", key.secret))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        if let Some(code) = code {
            let value: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(value["error"]["code"], code);
        }
    }
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let contract: serde_json::Value =
        serde_json::from_str(include_str!("../../api/openapi.yaml")).unwrap();
    for route in ["resume", "restart"] {
        let description = contract["paths"][format!("/api/v1/direct/acquisitions/{{id}}/{route}")]
            ["post"]["responses"]["409"]["description"]
            .as_str()
            .unwrap();
        for code in [
            "direct_review_conflict",
            "direct_busy",
            "direct_review_required",
            "transfer_resume_unavailable",
            "transfer_restart_required",
            "local_review",
            "source_changed",
            "direct_conflict",
            "manual_action",
            "challenge_required",
            "not_configured",
        ] {
            assert!(description.contains(code), "{route}: {code}");
        }
    }
}

#[tokio::test]
async fn reviewed_actions_keep_worker_configuration_and_import_fences() {
    let _serial = TEST_GATE.lock().await;
    let mut f = Fixture::new().await;
    let server = RangeFixture::new(f.store.clone(), Behavior::Resume).await;
    f.direct.fixture = Some(server.address);
    let intent = f.enqueue("review-fences").await;
    f.tick().await;
    let review = f.direct.review("owner", &intent.id).await.unwrap();
    let slot = SLOT.get().unwrap().lock().await;
    assert!(matches!(
        f.direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::Busy)
    ));
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::Busy)
    ));
    drop(slot);
    let process = f.direct.worker_lock().await.unwrap().unwrap();
    assert!(matches!(
        f.direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::Busy)
    ));
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::Busy)
    ));
    drop(process);
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE integrations SET enabled=0 WHERE id=?")
        .bind(&f.integration)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Configuration changes do not increment the acquisition revision, but invalidate its fingerprint.
    let changed = f.direct.review("owner", &intent.id).await.unwrap();
    assert_eq!(changed.revision, review.revision);
    assert_ne!(changed.transfer_fingerprint, review.transfer_fingerprint);
    assert!(matches!(
        f.direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::ReviewConflict)
    ));
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::ReviewConflict)
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 1);

    // Change configuration after preparation, during URL re-resolution. Only the writer's
    // evidence comparison can reject this snapshot; the acquisition revision is unchanged.
    for restart in [false, true] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), Behavior::Resume).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("resolution-race").await;
        f.tick().await;
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        f.server.seen.lock().unwrap().disable_during_resolution = true;
        let result = if restart {
            f.direct
                .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await
        } else {
            f.direct
                .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await
        };
        assert!(
            matches!(result, Err(DirectError::ReviewConflict)),
            "{result:?}"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        let current = f.direct.review("owner", &intent.id).await.unwrap();
        assert_eq!(current.revision, review.revision);
        assert_eq!(current.acquisition.state, "needs_review");
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM direct_transfer_events")
            .fetch_one(f.store.reader())
            .await
            .unwrap();
        assert_eq!(events, 0);
    }

    let f = Fixture::new().await;
    let intent = f.enqueue("journal-fence").await;
    f.tick().await;
    ImportService::new(f.store.clone())
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
            ImportMode::Standalone,
        )
        .await
        .unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE direct_acquisitions SET state='needs_review',reason='interrupted_transfer',source_relative=NULL WHERE id=?").bind(&intent.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let review = f.direct.review("owner", &intent.id).await.unwrap();
    assert_eq!(
        review.restart.reason.as_deref(),
        Some("import_review_required")
    );
    assert!(matches!(
        f.direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::ReviewRequired)
    ));
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await,
        Err(DirectError::ReviewRequired)
    ));
    assert_eq!(f.server.seen.lock().unwrap().transfers, 1);
}
