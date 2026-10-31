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
    WeakDate,
    MissingDate,
    Twice,
    FirstPageCut,
    Paused,
    Large,
    FreshAll,
}
struct RangeFixture {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
    release: Arc<tokio::sync::Notify>,
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
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let task = tokio::spawn(async move {
            let archive = if manga {
                let mut png = std::io::Cursor::new(Vec::new());
                image::DynamicImage::new_rgb8(2, 2)
                    .write_to(&mut png, image::ImageFormat::Png)
                    .unwrap();
                png.into_inner()
            } else {
                let mut bytes = ARCHIVE.to_vec();
                if behavior == Behavior::Large {
                    bytes.resize(40 * 1024 * 1024, 0);
                }
                bytes
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
                let number = seen.lock().unwrap().len();
                let cut = bytes.len() / 2;
                let etag = match behavior {
                    Behavior::NoValidators
                    | Behavior::DateOnly
                    | Behavior::WeakDate
                    | Behavior::MissingDate => "",
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
                        let requested = request
                            .lines()
                            .find_map(|line| line.strip_prefix("range: bytes="))
                            .unwrap()
                            .trim_end_matches('-')
                            .parse::<usize>()
                            .unwrap();
                        assert_eq!(
                            requested,
                            if behavior == Behavior::Twice && number == 3 {
                                cut + (bytes.len() - cut) / 2
                            } else {
                                cut
                            }
                        );
                        assert!(request.contains(if behavior == Behavior::DateOnly {
                            "if-range: Wed, 21 Oct 2026 10:00:00 GMT"
                        } else {
                            "if-range: \"owned\""
                        }));
                        let start = if behavior == Behavior::WrongStart {
                            requested + 1
                        } else {
                            requested
                        };
                        let total = bytes.len() + usize::from(behavior == Behavior::ChangedLength);
                        (
                            206,
                            bytes.len() - requested,
                            format!("Content-Range: bytes {start}-{}/{total}\r\n", total - 1),
                            if behavior == Behavior::Twice && number == 2 {
                                &bytes[requested..requested + (bytes.len() - requested) / 2]
                            } else {
                                &bytes[requested..]
                            },
                        )
                    } else {
                        (
                            200,
                            bytes.len(),
                            String::new(),
                            if is_resume
                                || (manga
                                    && (behavior == Behavior::FreshAll
                                        || number > 2
                                        || (request.contains("first.png")
                                            && behavior != Behavior::FirstPageCut)))
                            {
                                bytes
                            } else {
                                &bytes[..cut]
                            },
                        )
                    };
                let date = match behavior {
                    Behavior::MissingDate => "",
                    Behavior::WeakDate => "Date: Wed, 21 Oct 2026 10:00:00 GMT\r\n",
                    _ => "Date: Fri, 23 Oct 2026 10:00:00 GMT\r\n",
                };
                let headers = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {}\r\nContent-Length: {length}\r\n{date}{etag}{modified}{ranges}{content_range}Connection: close\r\n\r\n",
                    if manga {
                        "image/png"
                    } else {
                        "application/zip"
                    }
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                if behavior == Behavior::Paused && is_resume {
                    released.notified().await;
                }
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        Self {
            address,
            requests,
            task,
            release,
        }
    }
}
async fn wait_idle(direct: &Direct, owner: &str, id: &str) -> DirectAcquisition {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = direct.get(owner, id).await.unwrap();
            if state.state != "downloading"
                && let Ok(slot) = SLOT.get().unwrap().try_lock()
                && let Some(process) = direct.worker_lock().await.unwrap()
            {
                drop(process);
                drop(slot);
                return state;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("owned transfer must finish")
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
        Behavior::WeakDate,
        Behavior::MissingDate,
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
            Behavior::NoValidators
                | Behavior::WeakEtag
                | Behavior::NoRanges
                | Behavior::WeakDate
                | Behavior::MissingDate
        );
        assert_eq!(review.resume.eligible, !unavailable, "{behavior:?}");
        if matches!(behavior, Behavior::WeakDate | Behavior::MissingDate) {
            assert_eq!(
                review.resume.reason.as_deref(),
                Some("transfer_validator_not_strong")
            );
        }
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
    for change in ["tamper", "truncate", "origin"] {
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
    assert_eq!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await
            .unwrap()
            .id,
        next.id
    );
    let mut stale = action(&review);
    stale.transfer_fingerprint = "0".repeat(64);
    assert!(matches!(
        f.direct
            .restart_transfer(&f.settings, "owner", &intent.id, stale)
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
    let server = RangeFixture::new(f.store.clone(), Behavior::Paused).await;
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
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            app.clone().oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/direct/acquisitions/{path}"))
                    .header("authorization", format!("Bearer {}", key.secret))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            ),
        )
        .await
        .expect("202 must not await transfer completion")
        .unwrap();
        assert_eq!(response.status(), status, "{path}");
        if status == StatusCode::ACCEPTED {
            let value: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(value["state"], "downloading");
            loop {
                if server.requests.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            server.release.notify_one();
            wait_idle(&f.direct, &principal.user_id, &intent.id).await;
            continue;
        }
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

#[tokio::test]
async fn resumed_transfer_can_interrupt_again_and_trim_only_a_verified_tail() {
    let _serial = TEST_GATE.lock().await;
    for tampered in [false, true] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), Behavior::Twice).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("two-interruptions").await;
        f.tick().await;
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            f.direct
                .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await,
            Err(DirectError::Transfer)
        ));
        let partial = f
            .source
            .path()
            .join(format!(".library-downloads/{}/filepartial", intent.id));
        let before = std::fs::read(&partial).unwrap();
        assert_eq!(
            before.len(),
            ARCHIVE.len() / 2 + (ARCHIVE.len() - ARCHIVE.len() / 2) / 2
        );
        assert_eq!(before, &ARCHIVE[..before.len()]);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .unwrap();
        file.write_all(b"uncommitted tail").unwrap();
        file.sync_all().unwrap();
        if tampered {
            let mut bytes = std::fs::read(&partial).unwrap();
            bytes[0] ^= 1;
            std::fs::write(&partial, bytes).unwrap();
        }
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        // GET is deliberately cheap: content verification belongs to the action.
        assert!(review.resume.eligible);
        let result = f
            .direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await;
        if tampered {
            assert!(matches!(result, Err(DirectError::RestartRequired)));
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            assert_eq!(
                std::fs::metadata(&partial).unwrap().len(),
                (before.len() + 16) as u64
            );
        } else {
            assert_eq!(result.unwrap().state, "downloaded");
            assert_eq!(server.requests.lock().unwrap().len(), 3);
            assert_eq!(
                std::fs::read(partial.with_file_name("file.cbz")).unwrap(),
                ARCHIVE
            );
        }
    }
}

#[tokio::test]
async fn manga_rotation_and_never_started_middle_page_use_fresh_manifest_locations() {
    let _serial = TEST_GATE.lock().await;
    for unstarted in [false, true] {
        let mut f = Fixture::manga().await;
        if unstarted {
            f.server.seen.lock().unwrap().three_pages = true;
            f.request.link_handle = f.chapter_page().await.chapters[0].link_handle.clone();
        }
        let first = RangeFixture::with_pages(
            f.store.clone(),
            if unstarted {
                Behavior::FirstPageCut
            } else {
                Behavior::Resume
            },
            true,
        )
        .await;
        f.direct.fixture = Some(first.address);
        let intent = f.enqueue("node-rotation").await;
        f.tick().await;
        let before:i64=sqlx::query_scalar("SELECT COUNT(*) FROM direct_transfer_parts WHERE acquisition_id=? AND checkpoint IS NULL").bind(&intent.id).fetch_one(f.store.reader()).await.unwrap();
        assert_eq!(before, if unstarted { 2 } else { 0 });
        if !unstarted {
            let raw: String = sqlx::query_scalar(
                "SELECT checkpoint FROM direct_transfer_parts WHERE acquisition_id=? AND ordinal=1",
            )
            .bind(&intent.id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
            let mut complete: crate::acquisition::transfer_checkpoint::Checkpoint =
                serde_json::from_str(&raw).unwrap();
            assert!(complete.complete);
            complete.etag = None;
            complete.last_modified = Some("Wed, 21 Oct 2026 10:00:00 GMT".into());
            complete.last_modified_strong = false;
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query("UPDATE direct_transfer_parts SET checkpoint=? WHERE acquisition_id=? AND ordinal=1")
                .bind(serde_json::to_string(&complete).unwrap()).bind(&intent.id)
                .execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
        if unstarted {
            std::fs::remove_file(f.source.path().join(format!(
                ".library-downloads/{}/page-000002.partial",
                intent.id
            )))
            .unwrap();
        }
        f.server.seen.lock().unwrap().rotating_node = true;
        let second = RangeFixture::with_pages(f.store.clone(), Behavior::FreshAll, true).await;
        f.direct.fixture = Some(second.address);
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        assert!(review.resume.eligible, "{review:?}");
        let result = f
            .direct
            .resume_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await
            .unwrap();
        assert_eq!(result.state, "downloaded");
        assert_eq!(
            first.requests.lock().unwrap().len(),
            if unstarted { 1 } else { 2 }
        );
        assert_eq!(
            second.requests.lock().unwrap().len(),
            if unstarted { 3 } else { 1 }
        );
        let requests = second.requests.lock().unwrap();
        assert!(requests.iter().all(|r| r.contains("/rotated-token/data/")));
        assert!(requests[0].contains("range: bytes="));
        if unstarted {
            assert!(requests[1..].iter().all(|r| !r.contains("range:")));
        }
    }
}

#[tokio::test]
async fn corrupt_checkpoint_review_and_cleanup_failure_keep_restart_replay_available() {
    let _serial = TEST_GATE.lock().await;
    for unknown_field in [false, true] {
        let mut f = Fixture::new().await;
        let server = RangeFixture::new(f.store.clone(), Behavior::Resume).await;
        f.direct.fixture = Some(server.address);
        let intent = f.enqueue("invalid-checkpoint").await;
        f.tick().await;
        let staging = f
            .source
            .path()
            .join(format!(".library-downloads/{}", intent.id));
        let raw: String = sqlx::query_scalar(
            "SELECT checkpoint FROM direct_transfer_parts WHERE acquisition_id=?",
        )
        .bind(&intent.id)
        .fetch_one(f.store.reader())
        .await
        .unwrap();
        let mut corrupt = if unknown_field {
            serde_json::from_str::<serde_json::Value>(&raw).unwrap()
        } else {
            serde_json::json!({})
        };
        if unknown_field {
            corrupt["unknown"] = true.into();
        }
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE direct_transfer_parts SET checkpoint=? WHERE acquisition_id=?")
            .bind(corrupt.to_string())
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        // Cleanup cannot discard a multiply linked partial. This is an owned fixture only.
        std::fs::hard_link(
            staging.join("filepartial"),
            staging.join("page-999999.partial"),
        )
        .unwrap();
        // Restore single-link main file so preparation succeeds; the extra partial remains linked elsewhere.
        let bytes = std::fs::read(staging.join("filepartial")).unwrap();
        std::fs::remove_file(staging.join("filepartial")).unwrap();
        std::fs::write(staging.join("filepartial"), &bytes).unwrap();
        std::fs::hard_link(
            staging.join("page-999999.partial"),
            staging.join("keep.cbz"),
        )
        .unwrap();
        let review = f.direct.review("owner", &intent.id).await.unwrap();
        assert!(!review.resume.eligible);
        assert_eq!(
            review.resume.reason.as_deref(),
            Some("transfer_checkpoint_invalid")
        );
        assert!(review.restart.eligible);
        assert_eq!(review.transfer_fingerprint.len(), 64);
        let replacement = f
            .direct
            .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
            .await
            .unwrap();
        assert_eq!(replacement.state, "queued");
        assert_eq!(std::fs::read(staging.join("keep.cbz")).unwrap(), bytes);
        assert_eq!(
            f.direct
                .restart_transfer(&f.settings, "owner", &intent.id, action(&review))
                .await
                .unwrap()
                .id,
            replacement.id
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM direct_acquisitions")
            .fetch_one(f.store.reader())
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn durable_offsets_follow_synced_batches_not_individual_response_chunks() {
    use crate::acquisition::transfer_checkpoint::CHECKPOINT_TRACE;
    let _serial = TEST_GATE.lock().await;
    let mut f = Fixture::new().await;
    let server = RangeFixture::new(f.store.clone(), Behavior::Large).await;
    f.direct.fixture = Some(server.address);
    let intent = f.enqueue("batched-checkpoint").await;
    CHECKPOINT_TRACE.lock().unwrap().clear();
    f.tick().await;
    let raw: String =
        sqlx::query_scalar("SELECT checkpoint FROM direct_transfer_parts WHERE acquisition_id=?")
            .bind(&intent.id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    let cp: crate::acquisition::transfer_checkpoint::Checkpoint =
        serde_json::from_str(&raw).unwrap();
    assert_eq!(cp.offset, 20 * 1024 * 1024);
    let mut synced = 0;
    let mut saves = 0;
    for (is_sync, id, offset) in CHECKPOINT_TRACE.lock().unwrap().iter() {
        if id != &cp.file_identity {
            continue;
        }
        if *is_sync {
            synced = *offset;
        } else {
            assert!(*offset <= synced, "saved offset exceeds synced bytes");
            saves += 1;
        }
    }
    assert!(
        (3..=5).contains(&saves),
        "one checkpoint per batch, not per response chunk: {saves}"
    );
    assert_eq!(synced, cp.offset);
    assert_eq!(
        std::fs::metadata(
            f.source
                .path()
                .join(format!(".library-downloads/{}/filepartial", intent.id))
        )
        .unwrap()
        .len(),
        cp.offset
    );
}
