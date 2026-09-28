// Compile the adapters independently until root adds the library module export.
#[path = "../src/clients/mod.rs"]
mod clients;
use clients::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

const CLIENT: Uuid = Uuid::from_u128(1);
const OWN: Uuid = Uuid::from_u128(2);
const HASH: &str = "0123456789012345678901234567890123456789";
const NZO: &str = "SABnzbd_nzo_abc";
struct Step {
    path: &'static str,
    required: Vec<String>,
    response: Option<String>,
    stall: bool,
}
fn step(path: &'static str, required: &[&str], body: &str) -> Step {
    Step {
        path,
        stall: false,
        required: required.iter().map(|s| s.to_string()).collect(),
        response: Some(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )),
    }
}
fn login(legacy: bool) -> Step {
    let mut s = step(
        "POST /prefix/api/v2/auth/login",
        &[
            "username=user",
            "password=private-password",
            "origin:",
            "referer:",
        ],
        if legacy { "Ok." } else { "" },
    );
    s.response = Some(format!(
        "HTTP/1.1 {}\r\nSet-Cookie: random_session=opaque; Path=/prefix/; HttpOnly\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if legacy { "200 OK" } else { "204 No Content" },
        if legacy { 3 } else { 0 },
        if legacy { "Ok." } else { "" }
    ));
    s
}
fn auth(legacy: bool) -> Vec<Step> {
    vec![
        login(legacy),
        step(
            "GET /prefix/api/v2/app/version",
            &["cookie: random_session=opaque"],
            if legacy { "v4.6.7" } else { "v5.2.3" },
        ),
    ]
}
fn torrent(category: &str, tags: &str, state: &str) -> String {
    serde_json::json!([{ "hash": HASH, "category": category, "tags": tags, "state": state, "name": "https://secret/?apikey=private" }]).to_string()
}
fn tag() -> String {
    format!("libraryd-{OWN}")
}
fn sab_slot(history: bool, category: &str, state: &str) -> String {
    let key = if history { "history" } else { "queue" };
    let category_key = if history { "category" } else { "cat" };
    serde_json::json!({key: {"slots": [{"nzo_id": NZO, category_key: category, "status": state, "storage": "https://secret/?apikey=private"}]}}).to_string()
}
struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
    seen: Arc<Mutex<usize>>,
    finish_guard: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(steps: Vec<Step>) -> Self {
        Self::with_request_guard(steps, false).await
    }
    async fn with_request_guard(steps: Vec<Step>, guard: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/prefix/", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(0));
        let count = seen.clone();
        let (finish_guard, mut finish) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            for step in steps {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 8192];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .map(|s| s.parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with(step.path), "unexpected request target");
                for part in step.required {
                    assert!(
                        request.contains(&part),
                        "missing expected request field: {part}"
                    );
                }
                *count.lock().unwrap() += 1;
                if let Some(response) = step.response {
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
                if step.stall {
                    std::future::pending::<()>().await;
                }
            }
            if guard {
                // Keep accepting while enqueue/replay run, then observe a short grace
                // period. An extra lookup must not hide behind connection refusal.
                let mut finishing = false;
                let mut deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    tokio::select! {
                        _ = &mut finish, if !finishing => {
                            finishing = true;
                            deadline = tokio::time::Instant::now() + Duration::from_millis(100);
                        }
                        result = listener.accept() => {
                            let (mut socket, _) = result.unwrap();
                            *count.lock().unwrap() += 1;
                            let _ = tokio::time::timeout(Duration::from_millis(100), socket.write_all(
                                b"HTTP/1.1 500 Unexpected request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )).await;
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            assert!(finishing, "request guard expired before test completion");
                            break;
                        }
                    }
                }
            }
        });
        Self {
            url,
            task,
            seen,
            finish_guard: guard.then_some(finish_guard),
        }
    }
    fn config(&self) -> ClientConfig {
        self.config_limits(HttpLimits::default())
    }
    fn config_limits(&self, limits: HttpLimits) -> ClientConfig {
        ClientConfig::new(CLIENT, &self.url, "libraryd".into(), limits).unwrap()
    }
    async fn done(&mut self, n: usize) {
        if let Some(finish) = self.finish_guard.take() {
            finish.send(()).expect("request guard still active");
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*self.seen.lock().unwrap(), n);
    }
}
fn owned(kind: ClientKind) -> OwnedJob {
    OwnedJob::from_persisted_receipt(
        OWN,
        CLIENT,
        kind,
        "libraryd".into(),
        if kind == ClientKind::Sabnzbd {
            NZO
        } else {
            HASH
        }
        .into(),
    )
    .unwrap()
}
fn attempt(previous: bool) -> SubmissionAttempt {
    SubmissionAttempt::from_persisted(OWN, previous).unwrap()
}
fn nzb() -> AuthorizedPayload {
    AuthorizedPayload::nzb(b"<nzb/>".to_vec()).unwrap()
}
fn payload() -> AuthorizedPayload {
    AuthorizedPayload::torrent(b"d4:infodee".to_vec(), HASH.into()).unwrap()
}

#[tokio::test]
async fn qbit_accepts_204_cookie_and_legacy_200() {
    for legacy in [false, true] {
        let mut server = Server::new(auth(legacy)).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let info = client.test_connection().await.unwrap();
        assert_eq!(info.version, if legacy { "4.6.7" } else { "5.2.3" });
        server.done(2).await;
    }
}
#[tokio::test]
async fn qbit_login_alone_is_not_authentication() {
    let mut steps = vec![login(false)];
    let mut forbidden = step("GET /prefix/api/v2/app/version", &[], "private-password");
    forbidden.response =
        Some("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
    steps.push(forbidden);
    let mut server = Server::new(steps).await;
    let client =
        QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
    assert_eq!(
        client.test_connection().await,
        Err(ClientError::Authentication)
    );
    server.done(2).await;
}
#[tokio::test]
async fn qbit_enqueue_correlates_tag_category_hash_and_fences_replay() {
    let mut steps = auth(false);
    steps.push(step("GET /prefix/api/v2/torrents/info?", &[HASH], "[]"));
    steps.push(step(
        "POST /prefix/api/v2/torrents/add",
        &[
            "name=\"category\"\r\n\r\nlibraryd",
            &tag(),
            "name=\"torrents\"",
            "d4:infodee",
        ],
        "Ok.",
    ));
    steps.push(step(
        "GET /prefix/api/v2/torrents/info?",
        &[HASH],
        &torrent("libraryd", &tag(), "downloading"),
    ));
    let mut server = Server::new(steps).await;
    let client =
        QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
    let mut a = attempt(false);
    let jobs = client.enqueue(&mut a, payload()).await.unwrap();
    assert_eq!(jobs, vec![owned(ClientKind::QBittorrent)]);
    assert!(a.was_attempted());
    assert_eq!(
        client.enqueue(&mut a, payload()).await,
        Err(ClientError::NeedsReview)
    );
    server.done(5).await;
}
#[tokio::test]
async fn qbit_json_and_legacy_add_receipts_preserve_ownership_and_fence() {
    let receipt = serde_json::json!({"success_count":1,"failure_count":0,
        "pending_count":0,"added_torrent_ids":[HASH]})
    .to_string();
    for body in [receipt.as_str(), "Ok.", ""] {
        let mut steps = auth(false);
        steps.push(step("GET /prefix/api/v2/torrents/info?", &[HASH], "[]"));
        steps.push(step("POST /prefix/api/v2/torrents/add", &[&tag()], body));
        steps.push(step(
            "GET /prefix/api/v2/torrents/info?",
            &[HASH],
            &torrent("libraryd", &tag(), "downloading"),
        ));
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let mut a = attempt(false);
        assert_eq!(
            client.enqueue(&mut a, payload()).await.unwrap(),
            vec![owned(ClientKind::QBittorrent)]
        );
        assert!(a.was_attempted());
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert_eq!(
            client.enqueue(&mut attempt(true), payload()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(5).await;
    }
}

#[tokio::test]
async fn qbit_invalid_json_add_receipts_are_fenced_before_ownership_lookup() {
    let valid = serde_json::json!({"success_count":1,"failure_count":0,
        "pending_count":0,"added_torrent_ids":[HASH]});
    let mut bodies = vec![
        "{".to_owned(),
        "null".to_owned(),
        "[]".to_owned(),
        format!(
            r#"{{"success_count":0,"success_count":1,"failure_count":0,"pending_count":0,"added_torrent_ids":["{HASH}"]}}"#
        ),
    ];
    bodies.push(serde_json::json!([1, 0, 0, [HASH]]).to_string());
    for (key, value) in [
        ("success_count", serde_json::json!(0)),
        ("success_count", serde_json::json!(2)),
        ("success_count", serde_json::json!("1")),
        ("success_count", serde_json::json!(1.0)),
        ("failure_count", serde_json::json!(1)),
        ("failure_count", serde_json::json!(-1)),
        ("pending_count", serde_json::json!(1)),
        ("pending_count", serde_json::json!(null)),
        ("added_torrent_ids", serde_json::json!([])),
        ("added_torrent_ids", serde_json::json!([HASH, HASH])),
        (
            "added_torrent_ids",
            serde_json::json!(["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]),
        ),
        (
            "added_torrent_ids",
            serde_json::json!([HASH, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]),
        ),
        ("added_torrent_ids", serde_json::json!([null])),
        ("added_torrent_ids", serde_json::json!(HASH)),
    ] {
        let mut receipt = valid.clone();
        receipt[key] = value;
        bodies.push(receipt.to_string());
    }
    for key in [
        "success_count",
        "failure_count",
        "pending_count",
        "added_torrent_ids",
    ] {
        let mut receipt = valid.clone();
        receipt.as_object_mut().unwrap().remove(key);
        bodies.push(receipt.to_string());
    }
    for body in bodies {
        let mut steps = auth(false);
        steps.push(step("GET /prefix/api/v2/torrents/info?", &[HASH], "[]"));
        steps.push(step("POST /prefix/api/v2/torrents/add", &[], &body));
        let mut server = Server::with_request_guard(steps, true).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let mut a = attempt(false);
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert!(a.was_attempted());
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert_eq!(
            client.enqueue(&mut attempt(true), payload()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(4).await;
    }
}

#[tokio::test]
async fn qbit_valid_json_add_receipt_still_requires_matching_ownership() {
    let receipt = serde_json::json!({"success_count":1,"failure_count":0,
        "pending_count":0,"added_torrent_ids":[HASH]})
    .to_string();
    for row in [
        "[]".to_owned(),
        torrent("personal", &tag(), "downloading"),
        torrent("libraryd", "foreign-tag", "downloading"),
    ] {
        let mut steps = auth(false);
        steps.push(step("GET /prefix/api/v2/torrents/info?", &[HASH], "[]"));
        steps.push(step("POST /prefix/api/v2/torrents/add", &[], &receipt));
        steps.push(step("GET /prefix/api/v2/torrents/info?", &[HASH], &row));
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let mut a = attempt(false);
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert!(a.was_attempted());
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(5).await;
    }
}

#[tokio::test]
async fn qbit_existing_torrent_is_never_adopted_or_readded() {
    let mut steps = auth(false);
    steps.push(step(
        "GET /prefix/api/v2/torrents/info?",
        &[],
        &torrent("personal", "", "uploading"),
    ));
    let mut server = Server::new(steps).await;
    let client =
        QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
    assert_eq!(
        client.enqueue(&mut attempt(false), payload()).await,
        Err(ClientError::NeedsReview)
    );
    server.done(3).await;
}
#[tokio::test]
async fn qbit_lost_add_response_and_missing_correlation_need_review() {
    for lost in [true, false] {
        let mut steps = auth(false);
        steps.push(step("GET /prefix/api/v2/torrents/info?", &[], "[]"));
        let mut add = step("POST /prefix/api/v2/torrents/add", &[], "Ok.");
        if lost {
            add.response = None;
        }
        steps.push(add);
        if !lost {
            steps.push(step("GET /prefix/api/v2/torrents/info?", &[], "[]"));
        }
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let mut a = attempt(false);
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert_eq!(
            client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(if lost { 4 } else { 5 }).await;
    }
}
#[tokio::test]
async fn qbit_commands_validate_ownership_and_preserve_files() {
    for (action, path, legacy) in [
        (0, "POST /prefix/api/v2/torrents/stop", false),
        (1, "POST /prefix/api/v2/torrents/start", false),
        (2, "POST /prefix/api/v2/torrents/delete", false),
        (0, "POST /prefix/api/v2/torrents/pause", true),
        (1, "POST /prefix/api/v2/torrents/resume", true),
    ] {
        let mut steps = auth(legacy);
        steps.push(step(
            "GET /prefix/api/v2/torrents/info?",
            &[HASH],
            &torrent("libraryd", &tag(), "downloading"),
        ));
        steps.push(step(path, &[HASH, "deleteFiles=false"], ""));
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let job = owned(ClientKind::QBittorrent);
        match action {
            0 => client.pause(&job).await,
            1 => client.resume(&job).await,
            _ => client.remove(&job).await,
        }
        .unwrap();
        server.done(4).await;
    }
    for (category, tags) in [("personal", tag()), ("libraryd", "another-tag".into())] {
        let mut steps = auth(false);
        steps.push(step(
            "GET /prefix/api/v2/torrents/info?",
            &[],
            &torrent(category, &tags, "uploading"),
        ));
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        assert_eq!(
            client.remove(&owned(ClientKind::QBittorrent)).await,
            Err(ClientError::NotOwned)
        );
        server.done(3).await;
    }
}
#[tokio::test]
async fn qbit_states_and_dtos_do_not_expose_remote_metadata() {
    for (remote, expected) in [
        ("uploading", DownloadState::Seeding),
        ("stoppedUP", DownloadState::Completed),
        ("stoppedDL", DownloadState::Paused),
        ("error", DownloadState::Failed),
        ("checkingDL", DownloadState::Processing),
        ("queuedDL", DownloadState::Queued),
        ("newState", DownloadState::Unknown),
    ] {
        let mut steps = auth(false);
        steps.push(step(
            "GET /prefix/api/v2/torrents/info?",
            &[],
            &torrent("libraryd", &tag(), remote),
        ));
        let mut server = Server::new(steps).await;
        let client =
            QBittorrent::new(server.config(), "user".into(), "private-password".into()).unwrap();
        let result = client
            .queue(&[owned(ClientKind::QBittorrent)])
            .await
            .unwrap();
        assert_eq!(result[0].state, expected);
        assert!(!serde_json::to_string(&result).unwrap().contains("apikey"));
        server.done(3).await;
    }
}
#[tokio::test]
async fn sab_checks_management_key_and_version() {
    let mut server = Server::new(vec![
        step(
            "POST /prefix/api",
            &["mode=version", "apikey=private-key"],
            r#"{"version":"5.1.0"}"#,
        ),
        step(
            "POST /prefix/api",
            &["mode=queue"],
            r#"{"queue":{"slots":[]}}"#,
        ),
        step(
            "POST /prefix/api",
            &["mode=history"],
            r#"{"history":{"slots":[]}}"#,
        ),
    ])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(client.test_connection().await.unwrap().version, "5.1.0");
    server.done(3).await;
}
#[tokio::test]
async fn sab_preserves_all_returned_ids_and_blocks_replay() {
    let mut server = Server::new(vec![step(
        "POST /prefix/api",
        &[
            "name=\"apikey\"",
            "name=\"nzbfile\"",
            "name=\"cat\"\r\n\r\nlibraryd",
            &tag(),
        ],
        r#"{"status":true,"nzo_ids":["SABnzbd_nzo_abc","SABnzbd_nzo_def"]}"#,
    )])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    let mut a = attempt(false);
    let jobs = client.enqueue(&mut a, nzb()).await.unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].own_id(), OWN);
    assert_eq!(jobs[1].external_id(), "SABnzbd_nzo_def");
    assert_eq!(
        client.enqueue(&mut a, nzb()).await,
        Err(ClientError::NeedsReview)
    );
    server.done(1).await;
}
#[tokio::test]
async fn sab_uuid_receipt_survives_restore_and_single_job_lifecycle() {
    const ID: &str = "12345678-1234-4234-8234-123456789abc";
    let reply = serde_json::json!({"status":true,"nzo_ids":[ID]}).to_string();
    let queue = serde_json::json!({"queue":{"slots":[{"nzo_id":ID,"cat":"libraryd","status":"Downloading"}]}}).to_string();
    let history = serde_json::json!({"history":{"slots":[{"nzo_id":ID,"category":"libraryd","status":"Completed"}]}}).to_string();
    let mut steps = vec![step(
        "POST /prefix/api",
        &["name=\"nzbfile\"", &tag()],
        &reply,
    )];
    for action in ["pause", "resume"] {
        steps.push(step(
            "POST /prefix/api",
            &["mode=queue", &format!("nzo_ids={ID}")],
            &queue,
        ));
        steps.push(step(
            "POST /prefix/api",
            &[
                &format!("name={action}"),
                &format!("value={ID}"),
                "del_files=0",
                "apikey=private-key",
            ],
            &reply,
        ));
    }
    steps.push(step(
        "POST /prefix/api",
        &["mode=queue", ID],
        r#"{"queue":{"slots":[]}}"#,
    ));
    steps.push(step("POST /prefix/api", &["mode=history", ID], &history));
    steps.push(step(
        "POST /prefix/api",
        &[
            "mode=history",
            "name=delete",
            &format!("value={ID}"),
            "del_files=0",
        ],
        &reply,
    ));
    let mut server = Server::with_request_guard(steps, true).await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    let mut attempt = attempt(false);
    let jobs = client.enqueue(&mut attempt, nzb()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].external_id(), ID);
    assert_eq!(jobs[0].own_id(), OWN);
    let restored = OwnedJob::from_persisted_receipt(
        OWN,
        CLIENT,
        ClientKind::Sabnzbd,
        "libraryd".into(),
        ID.into(),
    )
    .unwrap();
    assert_eq!(jobs[0], restored);
    client.pause(&restored).await.unwrap();
    client.resume(&restored).await.unwrap();
    client.remove(&restored).await.unwrap();
    assert_eq!(
        client.enqueue(&mut attempt, nzb()).await,
        Err(ClientError::NeedsReview)
    );
    let mut fenced = SubmissionAttempt::from_persisted(OWN, true).unwrap();
    assert_eq!(
        client.enqueue(&mut fenced, nzb()).await,
        Err(ClientError::NeedsReview)
    );
    server.done(8).await;
}

#[tokio::test]
async fn sab_uuid_receipt_still_rejects_foreign_category_and_mismatched_id() {
    const ID: &str = "12345678-1234-4234-8234-123456789abc";
    for (id, category) in [
        (ID, "personal"),
        ("12345678-1234-4234-8234-123456789abd", "libraryd"),
    ] {
        let body = serde_json::json!({"queue":{"slots":[{"nzo_id":id,"cat":category,"status":"Downloading"}]}}).to_string();
        let mut server =
            Server::with_request_guard(vec![step("POST /prefix/api", &[ID], &body)], true).await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let job = OwnedJob::from_persisted_receipt(
            OWN,
            CLIENT,
            ClientKind::Sabnzbd,
            "libraryd".into(),
            ID.into(),
        )
        .unwrap();
        assert_eq!(client.remove(&job).await, Err(ClientError::NotOwned));
        server.done(1).await;
    }
}

#[tokio::test]
async fn sab_invalid_uuid_receipts_are_fenced_and_never_normalized() {
    for id in [
        "00000000-0000-0000-0000-000000000000",
        "12345678-1234-4234-8234-123456789ABC",
        "12345678123442348234123456789abc",
        "{12345678-1234-4234-8234-123456789abc}",
        "urn:uuid:12345678-1234-4234-8234-123456789abc",
        "12345678-1234-4234-8234-123456789abc,all",
        "12345678-1234-4234-8234-123456789abc&value=all",
        "12345678-1234-4234-8234-123456789abc\n",
        "all",
        "*",
        "SABnzbd_nzo_",
        "SABnzbd_nzo_abc,all",
    ] {
        assert_eq!(
            OwnedJob::from_persisted_receipt(
                OWN,
                CLIENT,
                ClientKind::Sabnzbd,
                "libraryd".into(),
                id.into()
            ),
            Err(ClientError::InvalidRequest)
        );
        let body = serde_json::json!({"status":true,"nzo_ids":[id]}).to_string();
        let mut server =
            Server::with_request_guard(vec![step("POST /prefix/api", &[], &body)], true).await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let mut attempt = attempt(false);
        assert_eq!(
            client.enqueue(&mut attempt, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        assert!(attempt.was_attempted());
        assert_eq!(
            client.enqueue(&mut attempt, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(1).await;
    }
    for ids in [
        serde_json::json!([null]),
        serde_json::json!([12]),
        serde_json::json!([{}]),
        serde_json::json!("12345678-1234-4234-8234-123456789abc"),
        serde_json::json!([
            "12345678-1234-4234-8234-123456789abc",
            "12345678-1234-4234-8234-123456789abc"
        ]),
    ] {
        let body = serde_json::json!({"status":true,"nzo_ids":ids}).to_string();
        let mut server =
            Server::with_request_guard(vec![step("POST /prefix/api", &[], &body)], true).await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let mut attempt = attempt(false);
        assert_eq!(
            client.enqueue(&mut attempt, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        assert!(attempt.was_attempted());
        assert_eq!(
            client.enqueue(&mut attempt, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(1).await;
    }
}

#[tokio::test]
async fn sab_lost_or_malformed_add_response_is_not_retried() {
    for response in [
        None,
        Some("private-key invalid json"),
        Some(r#"{"status":true,"nzo_ids":[]}"#),
        Some(r#"{"status":true,"nzo_ids":["all"]}"#),
    ] {
        let mut s = step("POST /prefix/api", &[], response.unwrap_or(""));
        if response.is_none() {
            s.response = None;
        }
        let mut server = Server::new(vec![s]).await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let mut a = attempt(false);
        assert_eq!(
            client.enqueue(&mut a, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        assert_eq!(
            client.enqueue(&mut a, nzb()).await,
            Err(ClientError::NeedsReview)
        );
        server.done(1).await;
    }
}
#[tokio::test]
async fn sab_queue_history_states_and_missing_are_distinct() {
    for (state, expected) in [
        ("Completed", DownloadState::Completed),
        ("Failed", DownloadState::Failed),
        ("Repairing", DownloadState::Processing),
        ("Queued", DownloadState::Processing),
        ("future", DownloadState::Unknown),
    ] {
        let mut server = Server::new(vec![
            step(
                "POST /prefix/api",
                &["mode=queue", NZO],
                r#"{"queue":{"slots":[]}}"#,
            ),
            step(
                "POST /prefix/api",
                &["mode=history", NZO],
                &sab_slot(true, "libraryd", state),
            ),
        ])
        .await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let statuses = client.queue(&[owned(ClientKind::Sabnzbd)]).await.unwrap();
        assert_eq!(statuses[0].state, expected);
        server.done(2).await;
    }
    let mut server = Server::new(vec![
        step("POST /prefix/api", &[], r#"{"queue":{"slots":[]}}"#),
        step("POST /prefix/api", &[], r#"{"history":{"slots":[]}}"#),
    ])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.status(&owned(ClientKind::Sabnzbd)).await,
        Err(ClientError::NotFound)
    );
    server.done(2).await;
}
#[tokio::test]
async fn sab_commands_are_single_job_and_reject_foreign_category() {
    for action in ["pause", "resume", "delete"] {
        let reply = format!(r#"{{"status":true,"nzo_ids":["{NZO}"]}}"#);
        let mut server = Server::new(vec![
            step(
                "POST /prefix/api",
                &["mode=queue", NZO],
                &sab_slot(false, "libraryd", "Downloading"),
            ),
            step(
                "POST /prefix/api",
                &[
                    &format!("name={action}"),
                    &format!("value={NZO}"),
                    "del_files=0",
                ],
                &reply,
            ),
        ])
        .await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let job = owned(ClientKind::Sabnzbd);
        match action {
            "pause" => client.pause(&job).await,
            "resume" => client.resume(&job).await,
            _ => client.remove(&job).await,
        }
        .unwrap();
        server.done(2).await;
    }
    let mut server = Server::new(vec![step(
        "POST /prefix/api",
        &[],
        &sab_slot(false, "personal", "Downloading"),
    )])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.remove(&owned(ClientKind::Sabnzbd)).await,
        Err(ClientError::NotOwned)
    );
    server.done(1).await;
}
#[tokio::test]
async fn response_limits_redirects_and_errors_are_redacted() {
    for response in [
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/?apikey=private-key\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n",
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
    ] {
        let mut s = step("POST /prefix/api", &[], "");
        s.response = Some(response.into());
        let mut server = Server::new(vec![s]).await;
        let client = Sabnzbd::new(
            server.config_limits(HttpLimits {
                timeout: Duration::from_secs(1),
                max_response_bytes: 8,
            }),
            "private-key".into(),
        )
        .unwrap();
        let error = client.test_connection().await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Rejected | ClientError::BodyTooLarge
        ));
        let rendered = format!(
            "{error:?} {error} {}",
            serde_json::to_string(&error).unwrap()
        );
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("http"));
        server.done(1).await;
    }
}
#[tokio::test]
async fn stale_attempt_and_wrong_client_fail_before_network() {
    let server = Server::new(vec![]).await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.enqueue(&mut attempt(true), nzb()).await,
        Err(ClientError::NeedsReview)
    );
    assert_eq!(
        client.pause(&owned(ClientKind::QBittorrent)).await,
        Err(ClientError::NotOwned)
    );
}
#[test]
fn config_and_identity_validation_reject_bulk_selectors_and_secrets() {
    for url in [
        "https://user:secret@example.org/",
        "https://example.org/?apikey=secret",
        "https://example.org/#secret",
        "file:///tmp/x",
    ] {
        assert!(ClientConfig::new(CLIENT, url, "libraryd".into(), HttpLimits::default()).is_err());
    }
    for category in ["", "*", "all,personal", "a/b", "a\r\nb"] {
        assert!(
            ClientConfig::new(
                CLIENT,
                "http://localhost/",
                category.into(),
                HttpLimits::default()
            )
            .is_err()
        );
    }
    for kind in [ClientKind::Sabnzbd, ClientKind::QBittorrent] {
        for id in [
            "all",
            "failed",
            "abc|def",
            "abc,def",
            "https://secret/?apikey=secret",
        ] {
            assert!(
                OwnedJob::from_persisted_receipt(OWN, CLIENT, kind, "libraryd".into(), id.into())
                    .is_err()
            );
        }
    }
    assert!(AuthorizedPayload::nzb(Vec::new()).is_err());
    assert!(AuthorizedPayload::torrent(vec![1], "all".into()).is_err());
}

#[tokio::test]
async fn timeout_bounds_the_whole_body_and_mutations_need_review() {
    for mutation in [false, true] {
        let mut s = step("POST /prefix/api", &[], "");
        s.response = Some("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".into());
        s.stall = true;
        let server = Server::new(vec![s]).await;
        let client = Sabnzbd::new(
            server.config_limits(HttpLimits {
                timeout: Duration::from_millis(100),
                max_response_bytes: 1024,
            }),
            "private-key".into(),
        )
        .unwrap();
        let mut a = attempt(false);
        let error = tokio::time::timeout(Duration::from_secs(2), async {
            if mutation {
                client.enqueue(&mut a, nzb()).await.unwrap_err()
            } else {
                client.test_connection().await.unwrap_err()
            }
        })
        .await
        .expect("client timeout must include reading the body");
        assert_eq!(
            error,
            if mutation {
                ClientError::NeedsReview
            } else {
                ClientError::Unavailable
            }
        );
        if mutation {
            assert_eq!(
                client.enqueue(&mut a, nzb()).await,
                Err(ClientError::NeedsReview)
            );
        }
        assert_eq!(*server.seen.lock().unwrap(), 1);
    }
}
#[tokio::test]
async fn unavailable_history_is_not_a_missing_job() {
    let mut broken = step("POST /prefix/api", &["mode=history"], "");
    broken.response = None;
    let mut server = Server::new(vec![
        step(
            "POST /prefix/api",
            &["mode=queue"],
            r#"{"queue":{"slots":[]}}"#,
        ),
        broken,
    ])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.status(&owned(ClientKind::Sabnzbd)).await,
        Err(ClientError::Unavailable)
    );
    server.done(2).await;
}
#[tokio::test]
async fn sab_removes_completed_history_without_deleting_files() {
    let mut server = Server::new(vec![
        step(
            "POST /prefix/api",
            &["mode=queue"],
            r#"{"queue":{"slots":[]}}"#,
        ),
        step(
            "POST /prefix/api",
            &["mode=history"],
            &sab_slot(true, "libraryd", "Completed"),
        ),
        step(
            "POST /prefix/api",
            &["mode=history", "name=delete", "del_files=0", NZO],
            &format!(r#"{{"status":true,"nzo_ids":["{NZO}"]}}"#),
        ),
    ])
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    client.remove(&owned(ClientKind::Sabnzbd)).await.unwrap();
    server.done(3).await;
}

#[tokio::test]
async fn sab_restricted_key_is_an_authentication_failure_not_missing_job() {
    let mut server = Server::new(vec![
        step(
            "POST /prefix/api",
            &["mode=version"],
            r#"{"version":"5.1.0"}"#,
        ),
        step(
            "POST /prefix/api",
            &["mode=queue"],
            r#"{"status":false,"error":"API Key Incorrect"}"#,
        ),
    ])
    .await;
    let client = Sabnzbd::new(server.config(), "restricted-key".into()).unwrap();
    assert_eq!(
        client.test_connection().await,
        Err(ClientError::Authentication)
    );
    server.done(2).await;
}

#[tokio::test]
async fn sab_history_status_only_delete_requires_exact_receipt_disappearance() {
    for id in [NZO, "12345678-1234-4234-8234-123456789abc"] {
        let history = serde_json::json!({"history":{"slots":[{"nzo_id":id,"category":"libraryd","status":"Completed"}]}}).to_string();
        let mut server = Server::with_request_guard(
            vec![
                step(
                    "POST /prefix/api",
                    &["mode=queue", &format!("nzo_ids={id}")],
                    r#"{"queue":{"slots":[]}}"#,
                ),
                step("POST /prefix/api", &["mode=history", id], &history),
                step(
                    "POST /prefix/api",
                    &[
                        "mode=history",
                        "name=delete",
                        &format!("value={id}"),
                        "del_files=0",
                        "apikey=private-key",
                    ],
                    r#"{"status":true}"#,
                ),
                step(
                    "POST /prefix/api",
                    &["mode=queue", &format!("nzo_ids={id}")],
                    r#"{"queue":{"slots":[]}}"#,
                ),
                step(
                    "POST /prefix/api",
                    &["mode=history", &format!("nzo_ids={id}")],
                    r#"{"history":{"slots":[]}}"#,
                ),
            ],
            true,
        )
        .await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        let job = OwnedJob::from_persisted_receipt(
            OWN,
            CLIENT,
            ClientKind::Sabnzbd,
            "libraryd".into(),
            id.into(),
        )
        .unwrap();
        client.remove(&job).await.unwrap();
        server.done(5).await;
    }
}

#[tokio::test]
async fn sab_history_delete_uncertain_confirmation_never_repeats_mutation() {
    for confirmation in [
        Some(sab_slot(true, "libraryd", "Completed")),
        Some(sab_slot(true, "foreign", "Completed")),
        Some(r#"{"status":false,"error":"API Key Incorrect"}"#.into()),
        Some("not json".into()),
        None,
    ] {
        let mut last = step(
            "POST /prefix/api",
            &["mode=history", NZO],
            confirmation.as_deref().unwrap_or(""),
        );
        if confirmation.is_none() {
            last.response = None;
        }
        let mut server = Server::with_request_guard(
            vec![
                step(
                    "POST /prefix/api",
                    &["mode=queue", NZO],
                    r#"{"queue":{"slots":[]}}"#,
                ),
                step(
                    "POST /prefix/api",
                    &["mode=history", NZO],
                    &sab_slot(true, "libraryd", "Completed"),
                ),
                step(
                    "POST /prefix/api",
                    &[
                        "mode=history",
                        "name=delete",
                        "del_files=0",
                        &format!("value={NZO}"),
                    ],
                    r#"{"status":true}"#,
                ),
                step(
                    "POST /prefix/api",
                    &["mode=queue", NZO],
                    r#"{"queue":{"slots":[]}}"#,
                ),
                last,
            ],
            true,
        )
        .await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        assert_eq!(
            client.remove(&owned(ClientKind::Sabnzbd)).await,
            Err(ClientError::NeedsReview)
        );
        server.done(5).await;
    }
}

#[tokio::test]
async fn sab_history_delete_queue_reappearance_stays_uncertain() {
    let mut server = Server::with_request_guard(
        vec![
            step(
                "POST /prefix/api",
                &["mode=queue", NZO],
                r#"{"queue":{"slots":[]}}"#,
            ),
            step(
                "POST /prefix/api",
                &["mode=history", NZO],
                &sab_slot(true, "libraryd", "Completed"),
            ),
            step(
                "POST /prefix/api",
                &[
                    "mode=history",
                    "name=delete",
                    "del_files=0",
                    &format!("value={NZO}"),
                ],
                r#"{"status":true}"#,
            ),
            step(
                "POST /prefix/api",
                &["mode=queue", &format!("nzo_ids={NZO}")],
                &sab_slot(false, "libraryd", "Downloading"),
            ),
        ],
        true,
    )
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.remove(&owned(ClientKind::Sabnzbd)).await,
        Err(ClientError::NeedsReview)
    );
    server.done(4).await;
}

#[tokio::test]
async fn sab_history_malformed_delete_acknowledgment_stays_uncertain() {
    for body in [
        None,
        Some("not json"),
        Some(r#"{"status":false}"#),
        Some(r#"{"status":"true"}"#),
        Some(r#"{}"#),
        Some(r#"{"status":true,"nzo_ids":[]}"#),
        Some(r#"{"status":true,"nzo_ids":null}"#),
        Some(r#"{"status":true,"nzo_ids":["other"]}"#),
    ] {
        let mut deletion = step(
            "POST /prefix/api",
            &["name=delete", "del_files=0", &format!("value={NZO}")],
            body.unwrap_or(""),
        );
        if body.is_none() {
            deletion.response = None;
        }
        let mut server = Server::with_request_guard(
            vec![
                step(
                    "POST /prefix/api",
                    &["mode=queue"],
                    r#"{"queue":{"slots":[]}}"#,
                ),
                step(
                    "POST /prefix/api",
                    &["mode=history"],
                    &sab_slot(true, "libraryd", "Completed"),
                ),
                deletion,
            ],
            true,
        )
        .await;
        let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
        assert_eq!(
            client.remove(&owned(ClientKind::Sabnzbd)).await,
            Err(ClientError::NeedsReview)
        );
        server.done(3).await;
    }
}

#[tokio::test]
async fn sab_queue_delete_still_requires_receipt_array() {
    let mut server = Server::with_request_guard(
        vec![
            step(
                "POST /prefix/api",
                &["mode=queue"],
                &sab_slot(false, "libraryd", "Downloading"),
            ),
            step(
                "POST /prefix/api",
                &["mode=queue", "name=delete", "del_files=0"],
                r#"{"status":true}"#,
            ),
        ],
        true,
    )
    .await;
    let client = Sabnzbd::new(server.config(), "private-key".into()).unwrap();
    assert_eq!(
        client.remove(&owned(ClientKind::Sabnzbd)).await,
        Err(ClientError::NeedsReview)
    );
    server.done(2).await;
}
