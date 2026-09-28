use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response},
};
use libraryd::providers::{
    HttpLimits, ProviderConfig, ProviderError, SearchPage, internet_archive::*,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone)]
struct MockState {
    replies: Arc<Mutex<VecDeque<(u16, String)>>>,
    requests: Arc<Mutex<Vec<String>>>,
}
struct Mock {
    base: String,
    state: MockState,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn start(replies: Vec<(u16, String)>) -> Self {
        let state = MockState {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handler).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
    fn provider(&self) -> InternetArchive {
        self.limited(HttpLimits::default())
    }
    fn limited(&self, limits: HttpLimits) -> InternetArchive {
        InternetArchive::new(ProviderConfig::new(&self.base, None, limits).unwrap()).unwrap()
    }
}
async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    assert_eq!(request.method(), "GET");
    assert_eq!(request.headers()["accept"], "application/json");
    assert!(request.headers().get("authorization").is_none());
    state.requests.lock().await.push(request.uri().to_string());
    let (status, body) = state
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected request");
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("location", "/should-not-follow")
        .header("retry-after", "17")
        .body(Body::from(body))
        .unwrap()
}
fn metadata() -> Value {
    json!({"identifier":"Issue-01", "title":"A magazine", "mediatype":"texts",
        "collection":["magazine_rack", "other"], "date":"2016-02", "language":["english", "fr"]})
}
fn item() -> Value {
    json!({"metadata":metadata(), "files":[{"name":"Issue scans/Issue 01.pdf", "format":"Text PDF",
        "size":"1234", "sha1":"0123456789abcdef0123456789abcdef01234567"}]})
}
fn reply(value: Value) -> (u16, String) {
    (200, value.to_string())
}
fn page(docs: Vec<Value>, start: u32, total: u64) -> Value {
    json!({"response":{"numFound":total, "start":start, "docs":docs}})
}

#[tokio::test]
async fn search_is_scoped_escaped_paginated_and_preserves_uncertainty() {
    let mock = Mock::start(vec![
        reply(page(vec![metadata()], 0, 2)),
        reply(page(vec![metadata()], 1, 2)),
    ])
    .await;
    let provider = mock.provider();
    let first = provider
        .search("x\" OR *:*", SearchPage { number: 1, size: 1 })
        .await
        .unwrap();
    assert_eq!(first.next_page, Some(2));
    assert_eq!(first.items[0].dates, ["2016-02"]);
    assert_eq!(first.items[0].languages, ["english", "fr"]);
    assert!(first.items[0].countries.is_empty());
    assert!(first.items[0].issues.is_empty());
    assert_eq!(
        provider
            .search("x", SearchPage { number: 2, size: 1 })
            .await
            .unwrap()
            .next_page,
        None
    );
    let requests = mock.state.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let url = reqwest::Url::parse(&format!("http://example.invalid{}", requests[0])).unwrap();
    assert_eq!(url.path(), "/prefix/advancedsearch.php");
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(
        params["q"],
        "collection:magazine_rack AND mediatype:texts AND (title:\"x\\\" OR *:*\")"
    );
    assert_eq!(params["rows"], "1");
    assert_eq!(params["sort[]"], "identifier asc");
}

#[tokio::test]
async fn item_preserves_exact_file_identity_without_fetching_media() {
    let mock = Mock::start(vec![reply(item())]).await;
    let result = mock.provider().item("Issue-01").await.unwrap();
    let file = &result.files[0];
    assert_eq!(file.identifier, "Issue-01");
    assert_eq!(file.name, "Issue scans/Issue 01.pdf");
    assert_eq!(file.size, Some(1234));
    assert_eq!(file.eligibility, FileEligibility::Available);
    assert_eq!(
        file.download_url.as_deref(),
        Some("https://archive.org/download/Issue-01/Issue%20scans/Issue%2001.pdf")
    );
    assert_eq!(
        *mock.state.requests.lock().await,
        ["/prefix/metadata/Issue-01"]
    );
}

#[tokio::test]
async fn source_fields_and_changed_file_snapshot_remain_distinct() {
    let mut original = item();
    original["metadata"]["date"] = json!(["2016", "2016-02"]);
    original["metadata"]["country"] = json!("Canada");
    original["metadata"]["coverage"] = json!("North America");
    original["metadata"]["volume"] = json!("01");
    original["metadata"]["issue"] = json!("02/03");
    let mut changed = original.clone();
    changed["files"][0]["size"] = json!(2345);
    let mock = Mock::start(vec![reply(original), reply(changed)]).await;
    let provider = mock.provider();
    let first = provider.item("Issue-01").await.unwrap();
    let second = provider.item("Issue-01").await.unwrap();
    assert_eq!(first.magazine.dates, ["2016", "2016-02"]);
    assert_eq!(first.magazine.countries, ["Canada"]);
    assert_eq!(first.magazine.coverage, ["North America"]);
    assert_eq!(first.magazine.volumes, ["01"]);
    assert_eq!(first.magazine.issues, ["02/03"]);
    assert_eq!(first.magazine, second.magazine);
    assert_ne!(first.files, second.files);
}

#[tokio::test]
async fn supported_formats_require_matching_names_and_preserve_md5_identity() {
    for (name, format) in [
        ("Issue.PDF", "Image Container PDF"),
        ("Issue.epub", "EPUB"),
        ("Issue.cbz", "Comic Book ZIP"),
    ] {
        let mut data = item();
        data["files"][0] = json!({
            "name": name, "format": format, "size": 1234,
            "md5": "0123456789ABCDEF0123456789ABCDEF", "private": false
        });
        let mock = Mock::start(vec![reply(data)]).await;
        let result = mock.provider().item("Issue-01").await.unwrap();
        let file = &result.files[0];
        assert_eq!(file.name, name);
        assert_eq!(
            file.md5.as_deref(),
            Some("0123456789ABCDEF0123456789ABCDEF")
        );
        assert_eq!(file.sha1, None);
        assert_eq!(file.eligibility, FileEligibility::Available);
    }
    for name in ["Issue.xml", "Issue_archive.torrent", "Issue.exe"] {
        let mut data = item();
        data["files"][0]["name"] = json!(name);
        let mock = Mock::start(vec![reply(data)]).await;
        let result = mock.provider().item("Issue-01").await.unwrap();
        assert_eq!(result.files[0].reason, EligibilityReason::UnsupportedFormat);
        assert!(result.files[0].download_url.is_none());
    }
}

#[tokio::test]
async fn pagination_stops_at_bound_and_accepts_empty_results() {
    let docs = (0..100)
        .map(|n| {
            let mut doc = metadata();
            doc["identifier"] = json!(format!("Issue-{n}"));
            doc
        })
        .collect();
    let mock = Mock::start(vec![
        reply(page(docs, 9900, 20_000)),
        reply(page(vec![], 0, 0)),
    ])
    .await;
    let provider = mock.provider();
    let last = provider
        .search(
            "Issue",
            SearchPage {
                number: 100,
                size: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(last.items.len(), 100);
    assert_eq!(last.total, 20_000);
    assert_eq!(last.next_page, None);
    let empty = provider
        .search("missing", SearchPage::default())
        .await
        .unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(empty.next_page, None);
    assert_eq!(mock.state.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn stalled_metadata_request_respects_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let state = MockState {
        replies: Arc::default(),
        requests: Arc::default(),
    };
    let task = tokio::spawn(async move {
        let app = Router::new().fallback(|| async { std::future::pending::<String>().await });
        axum::serve(listener, app).await.unwrap();
    });
    let mock = Mock { base, state, task };
    let provider = mock.limited(HttpLimits {
        timeout: Duration::from_millis(50),
        max_body_bytes: 4096,
    });
    let result = tokio::time::timeout(Duration::from_secs(2), provider.item("Issue-01"))
        .await
        .expect("provider deadline was not enforced");
    assert_eq!(result.unwrap_err(), ProviderError::Unavailable);
}

#[tokio::test]
async fn restrictions_and_auxiliary_files_are_never_available() {
    for (path, value, expected) in [
        (
            vec!["metadata", "access-restricted-item"],
            json!("true"),
            FileEligibility::Restricted,
        ),
        (
            vec!["nodownload"],
            json!(true),
            FileEligibility::Unavailable,
        ),
        (vec!["is_dark"], json!(1), FileEligibility::Unavailable),
        (
            vec!["servers_unavailable"],
            json!(true),
            FileEligibility::Unavailable,
        ),
    ] {
        let mut data = item();
        let mut target = &mut data;
        for key in path {
            target = &mut target[key];
        }
        *target = value;
        let mock = Mock::start(vec![reply(data)]).await;
        let result = mock.provider().item("Issue-01").await.unwrap();
        assert_eq!(result.files[0].eligibility, expected);
        assert!(result.files[0].download_url.is_none());
    }
    for (field, value, reason) in [
        (
            "private",
            json!("true"),
            EligibilityReason::AccessRestricted,
        ),
        (
            "format",
            json!("Metadata"),
            EligibilityReason::UnsupportedFormat,
        ),
        ("size", json!(null), EligibilityReason::IncompleteIdentity),
        ("sha1", json!(null), EligibilityReason::IncompleteIdentity),
    ] {
        let mut data = item();
        data["files"][0][field] = value;
        let mock = Mock::start(vec![reply(data)]).await;
        let file = mock
            .provider()
            .item("Issue-01")
            .await
            .unwrap()
            .files
            .remove(0);
        assert_eq!(file.reason, reason);
        assert!(file.download_url.is_none());
    }
    let mut data = item();
    data["files"] = json!([]);
    let mock = Mock::start(vec![reply(data)]).await;
    assert!(
        mock.provider()
            .item("Issue-01")
            .await
            .unwrap()
            .files
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_queries_do_not_make_requests() {
    let mock = Mock::start(vec![]).await;
    for id in ["../x", "https://elsewhere.invalid", "", "x%2fy"] {
        assert_eq!(
            mock.provider().item(id).await.unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
    for page in [
        SearchPage { number: 0, size: 1 },
        SearchPage {
            number: 1,
            size: 101,
        },
        SearchPage {
            number: 101,
            size: 100,
        },
    ] {
        assert_eq!(
            mock.provider().search("x", page).await.unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
    assert_eq!(
        mock.provider()
            .search("\n", SearchPage::default())
            .await
            .unwrap_err(),
        ProviderError::InvalidQuery
    );
    assert!(mock.state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn malformed_and_ambiguous_metadata_fail_closed() {
    let mut fixtures = vec![json!({}), json!({"metadata":metadata(), "files":null})];
    for (field, value) in [
        ("identifier", json!("issue-01")),
        ("collection", json!(["other"])),
        ("mediatype", json!("movies")),
        ("date", json!(123)),
        ("access-restricted-item", json!("unknown")),
    ] {
        let mut data = item();
        data["metadata"][field] = value;
        fixtures.push(data);
    }
    for (field, value) in [
        ("name", json!("../escape.pdf")),
        ("sha1", json!("bad")),
        ("size", json!("-1")),
        ("private", json!([])),
    ] {
        let mut data = item();
        data["files"][0][field] = value;
        fixtures.push(data);
    }
    let mut duplicate = item();
    duplicate["files"] = json!([duplicate["files"][0], duplicate["files"][0]]);
    fixtures.push(duplicate);
    let mut truncated = item();
    truncated["files_count"] = json!(2);
    fixtures.push(truncated);
    let mut oversized = item();
    oversized["files"] = json!(vec![oversized["files"][0].clone(); 2001]);
    fixtures.push(oversized);
    for fixture in fixtures {
        let mock = Mock::start(vec![reply(fixture)]).await;
        assert_eq!(
            mock.provider().item("Issue-01").await.unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
}

#[tokio::test]
async fn inconsistent_short_pages_fail_instead_of_silently_ending_search() {
    for (docs, start, total, number) in [
        (vec![metadata()], 0, 40, 1),
        (vec![], 0, 1, 1),
        (vec![metadata()], 20, 23, 2),
    ] {
        let mock = Mock::start(vec![reply(page(docs, start, total))]).await;
        assert_eq!(
            mock.provider()
                .search("x", SearchPage { number, size: 20 })
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse
        );
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
    let mock = Mock::start(vec![reply(page(vec![metadata()], 20, 21))]).await;
    let last = mock
        .provider()
        .search(
            "x",
            SearchPage {
                number: 2,
                size: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next_page, None);
}

#[tokio::test]
async fn pagination_and_transport_failures_are_bounded() {
    for data in [
        page(vec![metadata(), metadata()], 0, 2),
        page(vec![metadata()], 1, 2),
        page(vec![metadata()], 0, 0),
    ] {
        let mock = Mock::start(vec![reply(data)]).await;
        assert_eq!(
            mock.provider()
                .search("x", SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
    for (status, body, error) in [
        (302, "{}", ProviderError::Unavailable),
        (403, "{}", ProviderError::Unavailable),
        (
            429,
            "{}",
            ProviderError::RateLimited {
                retry_after_seconds: Some(17),
            },
        ),
        (200, "not json", ProviderError::InvalidResponse),
        (200, "[]", ProviderError::Unavailable),
    ] {
        let mock = Mock::start(vec![(status, body.into())]).await;
        assert_eq!(mock.provider().item("Issue-01").await.unwrap_err(), error);
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
    let mock = Mock::start(vec![reply(item())]).await;
    let provider = mock.limited(HttpLimits {
        timeout: Duration::from_secs(1),
        max_body_bytes: 32,
    });
    assert_eq!(
        provider.item("Issue-01").await.unwrap_err(),
        ProviderError::InvalidResponse
    );
}
