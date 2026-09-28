use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use libraryd::providers::*;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const KEY: &str = "test-key-ONLY";
const UUID: &str = "f6ffea82-617e-4e29-a843-289a761b419b";
const CAPS: &str = r#"<caps><limits max="100" default="20"/><searching><search available="yes" supportedParams="q"/></searching><categories><category id="7000"><subcat id="7030"/><subcat id="7010"/><subcat id="17001"/></category></categories></caps>"#;

struct Reply {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}
impl Reply {
    fn json(body: &str) -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type", "application/json".into())],
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
    fn xml(body: &str) -> Self {
        Self {
            headers: vec![("content-type", "application/xml".into())],
            ..Self::json(body)
        }
    }
    fn html(body: &str) -> Self {
        Self {
            headers: vec![("content-type", "text/html; charset=utf-8".into())],
            ..Self::json(body)
        }
    }
    fn status(mut self, status: u16) -> Self {
        self.status = StatusCode::from_u16(status).unwrap();
        self
    }
    fn header(mut self, key: &'static str, value: &str) -> Self {
        self.headers.push((key, value.into()));
        self
    }
}

#[derive(Clone)]
struct MockState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<Captured>>>,
}
struct Captured {
    method: String,
    uri: String,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
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
    async fn start(replies: Vec<Reply>) -> Self {
        let state = MockState {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new().fallback(handler).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
    fn config(&self, key: bool) -> ProviderConfig {
        ProviderConfig::new(
            &self.base,
            key.then(|| KEY.to_owned()),
            HttpLimits::default(),
        )
        .unwrap()
    }
    async fn assert_consumed(&self) {
        assert!(self.state.replies.lock().await.is_empty());
    }
}
async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 8192).await.unwrap().to_vec();
    state.requests.lock().await.push(Captured {
        method: parts.method.to_string(),
        uri: parts.uri.to_string(),
        headers: parts.headers,
        body,
    });
    let reply = state
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected HTTP request");
    tokio::time::sleep(reply.delay).await;
    let mut response = Response::builder().status(reply.status);
    for (key, value) in reply.headers {
        response = response.header(key, value);
    }
    response.body(Body::from(reply.body)).unwrap()
}
fn param(request: &Captured, name: &str) -> Option<String> {
    reqwest::Url::parse(&format!("http://example.invalid{}", request.uri))
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}
fn categories() -> CategoryMap {
    CategoryMap {
        comics: vec![7030],
        manga: vec![17001],
        magazines: vec![7010],
    }
}
fn indexer(mock: &Mock, protocol: ReleaseProtocol) -> Prowlarr {
    Prowlarr::new(mock.config(true), 7, protocol, categories()).unwrap()
}
fn cv_empty() -> &'static str {
    r#"{"status_code":1,"offset":0,"number_of_total_results":0,"results":[]}"#
}
fn mu_empty() -> &'static str {
    r#"{"page":1,"per_page":20,"total_hits":0,"results":[]}"#
}
fn md_empty() -> &'static str {
    r#"{"result":"ok","offset":0,"limit":20,"total":0,"data":[]}"#
}
async fn search(kind: u8, mock: &Mock) -> Result<MetadataPage, ProviderError> {
    match kind {
        0 => {
            ComicVine::new(mock.config(true))
                .unwrap()
                .search("A & B", SearchPage::default())
                .await
        }
        1 => {
            MangaUpdates::new(mock.config(false))
                .unwrap()
                .search("A & B", SearchPage::default())
                .await
        }
        _ => {
            MangaDex::new(mock.config(false))
                .unwrap()
                .search("A & B", SearchPage::default())
                .await
        }
    }
}

#[tokio::test]
async fn comic_vine_volume_search_normalizes_and_encodes_query() {
    let mock = Mock::start(vec![Reply::json(r#"{"status_code":1,"offset":2,"number_of_total_results":5,"results":[{"id":42,"name":"A & B","start_year":"2024","resource_type":"volume"}]}"#)]).await;
    let result = ComicVine::new(mock.config(true))
        .unwrap()
        .search("A & B?apikey=wrong", SearchPage { number: 2, size: 2 })
        .await
        .unwrap();
    assert_eq!(result.next_page, Some(3));
    assert_eq!(
        result.candidates[0],
        MetadataCandidate {
            provider: MetadataProvider::ComicVine,
            external_id: "4050-42".into(),
            title: "A & B".into(),
            content_type: ContentType::Comic,
            date: Some("2024".into())
        }
    );
    let requests = mock.state.requests.lock().await;
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert!(request.uri.starts_with("/prefix/search/?"));
    for (k, v) in [
        ("api_key", KEY),
        ("resources", "volume"),
        ("format", "json"),
        ("limit", "2"),
        ("offset", "2"),
        ("query", "A & B?apikey=wrong"),
    ] {
        assert_eq!(param(request, k).as_deref(), Some(v));
    }
    assert_eq!(request.headers["user-agent"], "libraryd/0.1");
    assert_eq!(request.headers["accept"], "application/json");
    assert!(!serde_json::to_string(&result).unwrap().contains(KEY));
}

#[tokio::test]
async fn manga_updates_posts_public_search_and_preserves_large_numeric_id() {
    let mock = Mock::start(vec![Reply::json(r#"{"page":1,"per_page":20,"total_hits":2,"results":[{"record":{"series_id":123456789012345,"title":"Example","type":"Manhwa","year":"2025"}},{"record":{"series_id":44,"title":"A novel","type":"Novel","year":"2025"}}]}"#)]).await;
    let result = search(1, &mock).await.unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].external_id, "123456789012345");
    assert_eq!(result.candidates[0].date.as_deref(), Some("2025"));
    assert_eq!(result.candidates[0].content_type, ContentType::Manga);
    let requests = mock.state.requests.lock().await;
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].uri, "/prefix/series/search");
    assert_eq!(requests[0].headers["content-type"], "application/json");
    assert!(!requests[0].headers.contains_key("authorization"));
    let payload: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(payload["search"], "A & B");
    assert_eq!(payload["page"], 1);
    assert_eq!(payload["perpage"], 20);
}

#[tokio::test]
async fn mangadex_uses_localized_title_and_publication_year_not_created_at() {
    let body = format!(
        r#"{{"result":"ok","offset":0,"limit":20,"total":1,"data":[{{"id":"{UUID}","type":"manga","attributes":{{"title":{{"ja":"日本語","en":""}},"originalLanguage":"ja","year":null,"createdAt":"2026-01-01T12:00:00Z"}}}}]}}"#
    );
    let mock = Mock::start(vec![Reply::json(&body)]).await;
    let result = search(2, &mock).await.unwrap();
    assert_eq!(result.candidates[0].title, "日本語");
    assert_eq!(result.candidates[0].external_id, UUID);
    assert_eq!(result.candidates[0].date, None);
    let requests = mock.state.requests.lock().await;
    assert!(requests[0].uri.starts_with("/prefix/manga?"));
    assert_eq!(param(&requests[0], "title").as_deref(), Some("A & B"));
    assert_eq!(param(&requests[0], "limit").as_deref(), Some("20"));
    assert_eq!(param(&requests[0], "offset").as_deref(), Some("0"));
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn metadata_empty_malformed_auth_and_429_have_distinct_safe_results() {
    for (kind, empty) in [(0, cv_empty()), (1, mu_empty()), (2, md_empty())] {
        let mock = Mock::start(vec![Reply::json(empty)]).await;
        assert!(search(kind, &mock).await.unwrap().candidates.is_empty());
        for body in ["not json", "{}", "{\"results\":null}"] {
            let mock = Mock::start(vec![Reply::json(body)]).await;
            assert_eq!(
                search(kind, &mock).await.unwrap_err(),
                ProviderError::InvalidResponse
            );
        }
        for (status, expected) in [
            (401, ProviderError::Unavailable),
            (403, ProviderError::Unavailable),
            (503, ProviderError::Unavailable),
            (
                429,
                ProviderError::RateLimited {
                    retry_after_seconds: Some(12),
                },
            ),
        ] {
            let mock = Mock::start(vec![
                Reply::json(&format!("private URL ?api_key={KEY}"))
                    .status(status)
                    .header("retry-after", "12"),
            ])
            .await;
            let error = search(kind, &mock).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?} {error}").contains(KEY));
            assert_eq!(mock.state.requests.lock().await.len(), 1);
        }
    }
}

fn rss(namespace: &str, mime: &str, offset: u32, total: u32, count: usize) -> String {
    let item = format!(
        r#"<item><title>A &amp; B</title><guid>release-42</guid><pubDate>Wed, 09 Sep 2026 00:00:00 GMT</pubDate><enclosure url="https://example.invalid/download?apikey={KEY}" length="12345" type="{mime}"/><x:attr name="category" value="7030"/><x:attr name="category" value="7030"/><x:attr name="category" value="7000"/><x:attr name="size" value="12345"/></item>"#
    );
    format!(
        r#"<rss xmlns:x="{namespace}"><channel><x:response offset="{offset}" total="{total}"/>{}</channel></rss>"#,
        item.repeat(count)
    )
}

#[tokio::test]
async fn torznab_and_newznab_caps_search_deduplicate_without_leaking_links() {
    for (protocol, namespace, mime) in [
        (
            ReleaseProtocol::Torrent,
            "http://torznab.com/schemas/2015/feed",
            "application/x-bittorrent",
        ),
        (
            ReleaseProtocol::Usenet,
            "http://www.newznab.com/DTD/2010/feeds/attributes/",
            "application/x-nzb",
        ),
    ] {
        let mock = Mock::start(vec![
            Reply::xml(CAPS),
            Reply::xml(&rss(namespace, mime, 20, 25, 2)),
        ])
        .await;
        let result = indexer(&mock, protocol)
            .search(
                "A & B",
                ContentType::Comic,
                SearchPage {
                    number: 2,
                    size: 20,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.releases.len(), 1);
        assert_eq!(result.next_offset, Some(22));
        assert_eq!(result.total, Some(25));
        let release = &result.releases[0];
        assert_eq!(release.indexer_id, 7);
        assert_eq!(release.guid, "release-42");
        assert_eq!(release.title, "A & B");
        assert_eq!(release.size_bytes, 12345);
        assert_eq!(release.categories, vec![7000, 7030]);
        assert_eq!(release.protocol, protocol);
        let dto = serde_json::to_string(&result).unwrap();
        assert!(!dto.contains(KEY));
        assert!(!format!("{result:?}").contains("download"));
        let requests = mock.state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        for req in requests.iter() {
            assert!(req.uri.starts_with("/prefix/7/api?"));
            assert_eq!(param(req, "apikey").as_deref(), Some(KEY));
            assert_eq!(req.headers["accept"], "application/xml");
        }
        assert_eq!(param(&requests[0], "t").as_deref(), Some("caps"));
        for (key, value) in [
            ("t", "search"),
            ("cat", "7030"),
            ("offset", "20"),
            ("limit", "20"),
            ("q", "A & B"),
        ] {
            assert_eq!(param(&requests[1], key).as_deref(), Some(value));
        }
    }
}

#[tokio::test]
async fn magazine_manga_categories_and_empty_indexer_results() {
    for (kind, cat) in [
        (ContentType::Magazine, "7010"),
        (ContentType::Manga, "17001"),
    ] {
        let mock = Mock::start(vec![Reply::xml(CAPS), Reply::xml("<rss><channel/></rss>")]).await;
        let result = indexer(&mock, ReleaseProtocol::Usenet)
            .search("title", kind, SearchPage::default())
            .await
            .unwrap();
        assert!(result.releases.is_empty());
        assert_eq!(result.next_offset, None);
        assert_eq!(result.total, None);
        assert_eq!(
            param(&mock.state.requests.lock().await[1], "cat").as_deref(),
            Some(cat)
        );
    }
}

#[tokio::test]
async fn unsupported_caps_never_trigger_search() {
    for caps in [
        CAPS.replace("available=\"yes\"", "available=\"no\""),
        CAPS.replace("supportedParams=\"q\"", "supportedParams=\"rid\""),
        CAPS.replace("7030", "7031"),
    ] {
        let mock = Mock::start(vec![Reply::xml(&caps)]).await;
        assert_eq!(
            indexer(&mock, ReleaseProtocol::Torrent)
                .search("title", ContentType::Comic, SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::Unsupported
        );
        mock.assert_consumed().await;
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn malformed_xml_and_protocol_errors_do_not_become_empty_success() {
    for body in [
        "",
        "<html/>",
        "<rss><channel>",
        "<rss><channel/></rss><rss/>",
        "<!DOCTYPE rss [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><rss><channel/></rss>",
        "<rss><channel><item><title>missing ID</title></item></channel></rss>",
        "<rss><channel/></rss>garbage",
    ] {
        let mock = Mock::start(vec![Reply::xml(CAPS), Reply::xml(body)]).await;
        assert_eq!(
            indexer(&mock, ReleaseProtocol::Usenet)
                .search("title", ContentType::Comic, SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse,
            "body: {body}"
        );
    }
    for (body, expected) in [
        (
            "<error code=\"100\" description=\"secret\"/>",
            ProviderError::Unavailable,
        ),
        (
            "<error code=\"429\" description=\"secret\"/>",
            ProviderError::RateLimited {
                retry_after_seconds: None,
            },
        ),
    ] {
        let mock = Mock::start(vec![Reply::xml(body)]).await;
        assert_eq!(
            indexer(&mock, ReleaseProtocol::Torrent)
                .capabilities()
                .await
                .unwrap_err(),
            expected
        );
    }
    let mock = Mock::start(vec![
        Reply::xml(CAPS),
        Reply::xml(&rss(
            "http://torznab.com/schemas/2015/feed",
            "application/x-bittorrent",
            0,
            1,
            1,
        )),
    ])
    .await;
    assert_eq!(
        indexer(&mock, ReleaseProtocol::Usenet)
            .search("title", ContentType::Comic, SearchPage::default())
            .await
            .unwrap_err(),
        ProviderError::InvalidResponse
    );
}

#[tokio::test]
async fn indexer_429_at_caps_or_search_propagates_retry_after() {
    for after_caps in [false, true] {
        let mut replies = vec![];
        if after_caps {
            replies.push(Reply::xml(CAPS));
        }
        replies.push(
            Reply::xml("<error code=\"429\"/>")
                .status(429)
                .header("retry-after", "30"),
        );
        let mock = Mock::start(replies).await;
        assert_eq!(
            indexer(&mock, ReleaseProtocol::Usenet)
                .search("title", ContentType::Comic, SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::RateLimited {
                retry_after_seconds: Some(30)
            }
        );
        mock.assert_consumed().await;
    }
}

#[tokio::test]
async fn retry_after_http_date_invalid_and_absent_are_safe() {
    for (header, expected) in [
        (Some("Wed, 01 Jan 2020 00:00:00 GMT"), Some(0)),
        (Some("https://example.invalid/?api_key=secret"), None),
        (None, None),
    ] {
        let mut reply = Reply::json("").status(429);
        if let Some(header) = header {
            reply = reply.header("retry-after", header);
        }
        let mock = Mock::start(vec![reply]).await;
        assert_eq!(
            search(0, &mock).await.unwrap_err(),
            ProviderError::RateLimited {
                retry_after_seconds: expected
            }
        );
    }
}

#[tokio::test]
async fn transport_bounds_body_time_and_redirects() {
    let target = Mock::start(vec![]).await;
    let mock = Mock::start(vec![
        Reply::json("").status(302).header("location", &target.base),
    ])
    .await;
    assert_eq!(
        search(0, &mock).await.unwrap_err(),
        ProviderError::Unavailable
    );
    assert!(target.state.requests.lock().await.is_empty());
    let mock = Mock::start(vec![Reply::json(&"x".repeat(1000))]).await;
    let config = ProviderConfig::new(
        &mock.base,
        None,
        HttpLimits {
            max_body_bytes: 100,
            ..HttpLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        MangaDex::new(config)
            .unwrap()
            .search("title", SearchPage::default())
            .await
            .unwrap_err(),
        ProviderError::InvalidResponse
    );
    let mut reply = Reply::json(md_empty());
    reply.delay = Duration::from_millis(200);
    let mock = Mock::start(vec![reply]).await;
    let config = ProviderConfig::new(
        &mock.base,
        None,
        HttpLimits {
            timeout: Duration::from_millis(20),
            ..HttpLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        MangaDex::new(config)
            .unwrap()
            .search("title", SearchPage::default())
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
}

#[tokio::test]
async fn getcomics_challenge_is_explicit_and_generic_cloudflare_script_is_not_one() {
    for reply in [
        Reply::html("<html>checking</html>").header("cf-mitigated", "challenge"),
        Reply::html("<html><form id='challenge-form'></form></html>").status(403),
        Reply::html(
            "<html><script>window._cf_chl_opt={}</script><script src='/cdn-cgi/challenge-platform/h/g/orchestrate/chl_page/v1'></script></html>",
        ),
    ] {
        let mock = Mock::start(vec![reply]).await;
        assert_eq!(
            GetComics::new(mock.config(false))
                .unwrap()
                .capability()
                .await
                .unwrap_err(),
            ProviderError::ChallengeRequired
        );
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
    let mock = Mock::start(vec![Reply::html("<html><title>GetComics</title><script src='/cdn-cgi/challenge-platform/scripts/jsd/main.js'></script><script src='https://static.cloudflareinsights.com/beacon.min.js'></script></html>")]).await;
    let result = GetComics::new(mock.config(false))
        .unwrap()
        .capability()
        .await
        .unwrap();
    assert!(result.page_reachable);
    assert!(!result.automated_search);
    assert!(!result.automated_download);
    assert_eq!(result.source_kind, "direct_download");
    assert_eq!(mock.state.requests.lock().await[0].uri, "/prefix/");
    for reply in [
        Reply::html("<html>unknown layout</html>"),
        Reply::json("{}"),
        Reply::html(""),
    ] {
        let mock = Mock::start(vec![reply]).await;
        assert_eq!(
            GetComics::new(mock.config(false))
                .unwrap()
                .capability()
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
}

#[test]
fn magazine_identity_is_local_or_valid_issn_and_never_an_issue_catalog() {
    let caps = magazine_capability();
    assert!(caps.local_manual && caps.issn_identity);
    assert!(!caps.external_metadata_search && !caps.universal_issue_catalog);
    let local = magazine_identity("local-1", "Magazine", None).unwrap();
    assert_eq!(local.provider, MetadataProvider::LocalManual);
    assert_eq!(local.content_type, ContentType::Magazine);
    assert_eq!(local.date, None);
    let issn = magazine_identity("local-1", "Magazine", Some("03178471")).unwrap();
    assert_eq!(issn.external_id, "0317-8471");
    assert_eq!(issn.provider, MetadataProvider::Issn);
    assert!(magazine_identity("local-1", "Magazine", Some("2434-561X")).is_ok());
    for issn in [
        "0317-8472",
        "not-an-issn",
        "1234-567８",
        "éé-1234",
        "1éé-1234",
    ] {
        assert_eq!(
            magazine_identity("local-1", "Magazine", Some(issn)).unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
}

#[tokio::test]
async fn bad_configuration_queries_and_reflected_credentials_are_rejected() {
    for base in [
        "file:///tmp/test",
        "ftp://example.invalid/",
        "http://user:secret@example.invalid/",
        "http://example.invalid/?apikey=secret",
        "http://example.invalid/#fragment",
    ] {
        assert!(ProviderConfig::new(base, None, HttpLimits::default()).is_err());
    }
    let mock = Mock::start(vec![]).await;
    let provider = MangaDex::new(mock.config(false)).unwrap();
    for page in [
        SearchPage {
            number: 0,
            size: 20,
        },
        SearchPage { number: 1, size: 0 },
        SearchPage {
            number: u32::MAX,
            size: 100,
        },
        SearchPage {
            number: 101,
            size: 100,
        },
    ] {
        assert_eq!(
            provider.search("title", page).await.unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
    assert_eq!(
        provider
            .search("", SearchPage::default())
            .await
            .unwrap_err(),
        ProviderError::InvalidQuery
    );
    assert!(mock.state.requests.lock().await.is_empty());
    let body = format!(
        r#"{{"status_code":1,"offset":0,"number_of_total_results":1,"results":[{{"id":42,"name":"{KEY}","resource_type":"volume"}}]}}"#
    );
    let mock = Mock::start(vec![Reply::json(&body)]).await;
    assert_eq!(
        search(0, &mock).await.unwrap_err(),
        ProviderError::InvalidResponse
    );
}

#[tokio::test]
async fn manga_updates_effective_page_size_is_carried_forward() {
    let mock = Mock::start(vec![Reply::json(r#"{"page":2,"per_page":25,"total_hits":30,"results":[{"record":{"series_id":42,"title":"Example","type":"Manga","year":"1997"}}]}"#)]).await;
    let result = MangaUpdates::new(mock.config(false))
        .unwrap()
        .search("title", SearchPage { number: 2, size: 1 })
        .await
        .unwrap();
    assert_eq!(result.page_size, 25);
    assert_eq!(result.next_page, Some(3));
    assert_eq!(result.candidates[0].date.as_deref(), Some("1997"));
}

#[tokio::test]
async fn acquisition_keeps_opaque_target_and_retrieves_only_configured_origin() {
    let mock = Mock::start(vec![]).await;
    let target = format!("{}7/get?apikey={KEY}&amp;id=42", mock.base);
    let response = rss(
        "http://www.newznab.com/DTD/2010/feeds/attributes/",
        "application/x-nzb",
        0,
        1,
        1,
    )
    .replace(
        &format!("https://example.invalid/download?apikey={KEY}"),
        &target,
    );
    mock.state.replies.lock().await.extend([
        Reply::xml(CAPS),
        Reply::xml(&response),
        Reply::xml("<nzb/>"),
    ]);
    let provider = indexer(&mock, ReleaseProtocol::Usenet);
    let page = provider
        .search_for_acquisition("title", ContentType::Comic, 0, 20)
        .await
        .unwrap();
    assert_eq!(page.releases.len(), 1);
    assert!(
        !serde_json::to_string(&page.releases[0].release)
            .unwrap()
            .contains(KEY)
    );
    assert!(!format!("{:?}", page.releases[0].release).contains(KEY));
    let download = page.releases[0].download.as_ref().unwrap();
    let other = Mock::start(vec![]).await;
    assert_eq!(
        indexer(&other, ReleaseProtocol::Usenet)
            .retrieve_payload(download)
            .await
            .unwrap_err(),
        ProviderError::Unsupported
    );
    assert!(other.state.requests.lock().await.is_empty());
    assert_eq!(
        provider.retrieve_payload(download).await.unwrap(),
        b"<nzb/>"
    );
    let requests = mock.state.requests.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(param(&requests[2], "apikey").as_deref(), Some(KEY));
    assert_eq!(param(&requests[2], "id").as_deref(), Some("42"));
}

#[tokio::test]
async fn acquisition_rejects_foreign_urls_and_does_not_follow_redirects() {
    let mock = Mock::start(vec![
        Reply::xml(CAPS),
        Reply::xml(&rss(
            "http://www.newznab.com/DTD/2010/feeds/attributes/",
            "application/x-nzb",
            0,
            1,
            1,
        )),
    ])
    .await;
    assert_eq!(
        indexer(&mock, ReleaseProtocol::Usenet)
            .search_for_acquisition("title", ContentType::Comic, 0, 20)
            .await
            .err(),
        Some(ProviderError::Unsupported)
    );
    let target = Mock::start(vec![]).await;
    let mock = Mock::start(vec![]).await;
    let response = rss(
        "http://www.newznab.com/DTD/2010/feeds/attributes/",
        "application/x-nzb",
        0,
        1,
        1,
    )
    .replace(
        &format!("https://example.invalid/download?apikey={KEY}"),
        &format!("{}7/get?apikey={KEY}", mock.base),
    );
    mock.state.replies.lock().await.extend([
        Reply::xml(CAPS),
        Reply::xml(&response),
        Reply::xml("").status(302).header("location", &target.base),
    ]);
    let provider = indexer(&mock, ReleaseProtocol::Usenet);
    let page = provider
        .search_for_acquisition("title", ContentType::Comic, 0, 20)
        .await
        .unwrap();
    assert_eq!(
        provider
            .retrieve_payload(page.releases[0].download.as_ref().unwrap())
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert!(target.state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn streaming_body_without_content_length_is_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for slow in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let mut used = 0;
            while !request[..used].windows(4).any(|part| part == b"\r\n\r\n") {
                assert!(used < request.len(), "mock request headers exceed bound");
                let read = socket.read(&mut request[used..]).await.unwrap();
                assert!(read > 0, "mock request ended before headers");
                used += read;
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n").await.unwrap();
            if slow {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let _ = socket.write_all(b"40\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n40\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n0\r\n\r\n").await;
        });
        let config = ProviderConfig::new(
            &base,
            None,
            HttpLimits {
                timeout: Duration::from_millis(150),
                max_body_bytes: 100,
            },
        )
        .unwrap();
        let result = MangaDex::new(config)
            .unwrap()
            .search("title", SearchPage::default())
            .await;
        assert_eq!(
            result.unwrap_err(),
            if slow {
                ProviderError::Unavailable
            } else {
                ProviderError::InvalidResponse
            }
        );
        task.abort();
    }
}

#[tokio::test]
#[ignore = "explicit opt-in public HTTP metadata search"]
async fn live_public_metadata_search() {
    let page = SearchPage { number: 1, size: 1 };
    let dex = MangaDex::new(
        ProviderConfig::new("https://api.mangadex.org/", None, HttpLimits::default()).unwrap(),
    )
    .unwrap();
    let dex_result = dex.search("One Piece", page).await.unwrap();
    assert!(!dex_result.candidates.is_empty());
    assert_eq!(dex_result.candidates[0].content_type, ContentType::Manga);
    let updates = MangaUpdates::new(
        ProviderConfig::new(
            "https://api.mangaupdates.com/v1/",
            None,
            HttpLimits::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let updates_result = updates.search("One Piece", page).await.unwrap();
    assert!(!updates_result.candidates.is_empty());
    assert!((1..=100).contains(&updates_result.page_size));
}

#[tokio::test]
#[ignore = "requires explicit provider credential file and live search authorization"]
async fn live_comic_vine_search() {
    let path = std::env::var("PROVIDER_COMICVINE_API_KEY_FILE")
        .expect("set PROVIDER_COMICVINE_API_KEY_FILE to a protected credential file");
    let key = std::fs::read_to_string(path).expect("read protected credential file");
    let config = ProviderConfig::new(
        "https://comicvine.gamespot.com/api/",
        Some(key.trim().to_owned()),
        HttpLimits::default(),
    )
    .unwrap();
    let result = ComicVine::new(config)
        .unwrap()
        .search("Batman", SearchPage { number: 1, size: 1 })
        .await
        .unwrap();
    assert!(!result.candidates.is_empty());
    assert!(result.candidates[0].external_id.starts_with("4050-"));
}
