use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Method, Request, StatusCode},
    response::{IntoResponse, Response},
};
use libraryd::clients::{
    AuthorizedPayload, ClientConfig, ClientError, HttpLimits, QBittorrent, SubmissionAttempt,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const HASH: &str = "0123456789012345678901234567890123456789";
const OWN: Uuid = Uuid::from_u128(2);

struct Mock {
    add_body: String,
    replies: Vec<(StatusCode, String)>,
    stall: bool,
    calls: AtomicUsize,
    adds: AtomicUsize,
    lookups: AtomicUsize,
    unexpected: AtomicUsize,
}

async fn handle(State(mock): State<Arc<Mock>>, request: Request<Body>) -> Response {
    mock.calls.fetch_add(1, Ordering::SeqCst);
    match (request.method(), request.uri().path()) {
        (&Method::POST, "/api/v2/auth/login") => "Ok.".into_response(),
        (&Method::GET, "/api/v2/app/version") => "v5.2.3".into_response(),
        (&Method::POST, "/api/v2/torrents/add") => {
            mock.adds.fetch_add(1, Ordering::SeqCst);
            mock.add_body.clone().into_response()
        }
        (&Method::GET, "/api/v2/torrents/info")
            if request.uri().query() == Some(format!("hashes={HASH}").as_str()) =>
        {
            if mock.adds.load(Ordering::SeqCst) == 0 {
                return "[]".into_response();
            }
            let index = mock.lookups.fetch_add(1, Ordering::SeqCst);
            if mock.stall {
                std::future::pending::<()>().await;
            }
            let (status, body) = &mock.replies[index.min(mock.replies.len() - 1)];
            (*status, body.clone()).into_response()
        }
        _ => {
            mock.unexpected.fetch_add(1, Ordering::SeqCst);
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

struct Fixture {
    client: QBittorrent,
    mock: Arc<Mock>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(add_body: String, replies: Vec<(StatusCode, String)>, stall: bool) -> Self {
        let mock = Arc::new(Mock {
            add_body,
            replies,
            stall,
            calls: AtomicUsize::new(0),
            adds: AtomicUsize::new(0),
            lookups: AtomicUsize::new(0),
            unexpected: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(mock.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = QBittorrent::new(
            ClientConfig::new(
                Uuid::from_u128(1),
                &base,
                "libraryd".into(),
                HttpLimits {
                    timeout: Duration::from_secs(10),
                    max_response_bytes: 4096,
                },
            )
            .unwrap(),
            "fixture".into(),
            "fixture".into(),
        )
        .unwrap();
        Self { client, mock, task }
    }
    async fn assert_fenced(&self, attempt: &mut SubmissionAttempt) {
        let calls = self.mock.calls.load(Ordering::SeqCst);
        assert_eq!(calls, 4 + self.mock.lookups.load(Ordering::SeqCst));
        assert!(attempt.was_attempted());
        assert_eq!(
            self.client.enqueue(attempt, payload()).await,
            Err(ClientError::NeedsReview)
        );
        let mut restored = SubmissionAttempt::from_persisted(OWN, true).unwrap();
        assert_eq!(
            self.client.enqueue(&mut restored, payload()).await,
            Err(ClientError::NeedsReview)
        );
        // The listener stays live: unexpected extra GETs/POSTs cannot hide behind refusal.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(self.mock.calls.load(Ordering::SeqCst), calls);
        assert_eq!(self.mock.adds.load(Ordering::SeqCst), 1);
        assert_eq!(self.mock.unexpected.load(Ordering::SeqCst), 0);
        assert!(!self.task.is_finished());
    }
}
fn payload() -> AuthorizedPayload {
    AuthorizedPayload::torrent(b"d4:infodee".to_vec(), HASH.into()).unwrap()
}
fn attempt() -> SubmissionAttempt {
    SubmissionAttempt::from_persisted(OWN, false).unwrap()
}
fn receipt() -> String {
    serde_json::json!({"success_count":1,"failure_count":0,"pending_count":0,
        "added_torrent_ids":[HASH]})
    .to_string()
}
fn row(tags: &str) -> String {
    serde_json::json!([{"hash":HASH,"category":"libraryd","tags":tags,"state":"downloading"}])
        .to_string()
}
fn ok(body: &str) -> (StatusCode, String) {
    (StatusCode::OK, body.into())
}

#[tokio::test]
async fn qbit_visibility_accepts_later_owned_row_with_one_add() {
    for add in [receipt(), "Ok.".into(), String::new()] {
        let f = Fixture::new(
            add,
            vec![ok("[]"), ok("[]"), ok(&row(&format!("libraryd-{OWN}")))],
            false,
        )
        .await;
        let mut a = attempt();
        let jobs = f.client.enqueue(&mut a, payload()).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].external_id(), HASH);
        assert_eq!(jobs[0].own_id(), OWN);
        assert_eq!(f.mock.lookups.load(Ordering::SeqCst), 3);
        f.assert_fenced(&mut a).await;
    }
}

#[tokio::test]
async fn qbit_visibility_rejects_non_absence_errors_without_polling() {
    for reply in [
        ok(&row("foreign")),
        ok("{}"),
        ok("not-json"),
        (StatusCode::FORBIDDEN, String::new()),
        (StatusCode::INTERNAL_SERVER_ERROR, String::new()),
    ] {
        // A later matching row would hide a security/protocol failure if retried.
        let f = Fixture::new(
            receipt(),
            vec![reply, ok(&row(&format!("libraryd-{OWN}")))],
            false,
        )
        .await;
        let mut a = attempt();
        assert_eq!(
            f.client.enqueue(&mut a, payload()).await,
            Err(ClientError::NeedsReview)
        );
        assert_eq!(f.mock.lookups.load(Ordering::SeqCst), 1);
        f.assert_fenced(&mut a).await;
    }
}

#[tokio::test]
async fn qbit_visibility_permanent_absence_and_stalled_get_are_bounded_and_fenced() {
    for stall in [false, true] {
        let f = Fixture::new(receipt(), vec![ok("[]")], stall).await;
        let mut a = attempt();
        let start = tokio::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(4), f.client.enqueue(&mut a, payload()))
                .await
                .expect("visibility deadline includes stalled HTTP request");
        assert_eq!(result, Err(ClientError::NeedsReview));
        assert!(start.elapsed() >= Duration::from_millis(1900));
        let lookups = f.mock.lookups.load(Ordering::SeqCst);
        if stall {
            assert_eq!(lookups, 1);
        } else {
            assert!((2..=21).contains(&lookups));
        }
        f.assert_fenced(&mut a).await;
    }
}

#[tokio::test]
async fn qbit_visibility_malformed_receipt_never_starts_lookup() {
    let f = Fixture::new(
        "{".into(),
        vec![ok(&row(&format!("libraryd-{OWN}")))],
        false,
    )
    .await;
    let mut a = attempt();
    assert_eq!(
        f.client.enqueue(&mut a, payload()).await,
        Err(ClientError::NeedsReview)
    );
    assert_eq!(f.mock.lookups.load(Ordering::SeqCst), 0);
    f.assert_fenced(&mut a).await;
}
