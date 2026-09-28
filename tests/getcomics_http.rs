use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response},
};
use libraryd::providers::{HttpLimits, ProviderConfig, ProviderError, getcomics::*};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const SEARCH: &str = r#"<!doctype html><html><h1 class=search-title>Search</h1>
<div class=post-list-posts>
<article><h1 class=post-title><a href='/dc/example-1/'>Example &amp; <i>Friends</i> #1</a></h1></article>
<article><h1 class=post-title><a href='/dc/example-1/'>Duplicate</a></h1></article>
<article><h1 class=post-title><a href='/news/announcement/'>News</a></h1></article></div>
<nav class=pagination><a href='/page/2/?s=example+friends'>2</a></nav></html>"#;

fn detail(links: &str) -> String {
    format!(
        "<html><h1 class=post-title>Example #1</h1><article class=post-body><section class=post-contents>{links}</section></article></html>"
    )
}
fn button(url: &str) -> String {
    format!("<a class=aio-red href='{url}'>Download</a>")
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    stall: bool,
}
impl Reply {
    fn html(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/html".into())],
            body: body.into(),
            stall: false,
        }
    }
    fn redirect(target: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("location", target.into())],
            ..Self::html("")
        }
    }
}
#[derive(Default)]
struct MockState {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(String, String, axum::http::HeaderMap)>>,
}
struct Mock {
    base: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn start(replies: Vec<Reply>) -> Self {
        let state = Arc::new(MockState {
            replies: Mutex::new(replies.into()),
            ..Default::default()
        });
        let app = Router::new()
            .fallback(
                |State(state): State<Arc<MockState>>, request: Request<Body>| async move {
                    state.requests.lock().await.push((
                        request.method().to_string(),
                        request.uri().to_string(),
                        request.headers().clone(),
                    ));
                    let reply = state
                        .replies
                        .lock()
                        .await
                        .pop_front()
                        .expect("unexpected network request");
                    if reply.stall {
                        std::future::pending::<()>().await;
                    }
                    let mut response = Response::builder().status(reply.status);
                    for (key, value) in reply.headers {
                        response = response.header(key, value);
                    }
                    response.body(Body::from(reply.body)).unwrap()
                },
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
    fn adapter(&self) -> GetComicsAdapter {
        self.with_limits(HttpLimits::default())
    }
    fn with_limits(&self, limits: HttpLimits) -> GetComicsAdapter {
        GetComicsAdapter::new(
            ProviderConfig::new(&self.base, Some("private-config-key".into()), limits).unwrap(),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn search_extracts_titles_deduplicates_posts_and_constructs_pagination() {
    let mock = Mock::start(vec![
        Reply::html(SEARCH),
        Reply::html(
            "<section class=post-list><nav class=pagination-noresults>No Result</nav></section>",
        ),
    ])
    .await;
    let adapter = mock.adapter();
    let page = adapter.search("example friends", 1).await.unwrap();
    assert_eq!(
        page.posts,
        vec![GetComicsPost {
            post_path: "/dc/example-1/".into(),
            title: "Example & Friends #1".into()
        }]
    );
    assert_eq!(page.next_page, Some(2));
    assert!(
        adapter
            .search("example friends", 2)
            .await
            .unwrap()
            .posts
            .is_empty()
    );
    let requests = mock.state.requests.lock().await;
    assert_eq!(requests[0].1, "/?s=example+friends");
    assert_eq!(requests[1].1, "/page/2/?s=example+friends");
}

#[tokio::test]
async fn invalid_queries_and_post_paths_never_request_network() {
    let mock = Mock::start(vec![]).await;
    let adapter = mock.adapter();
    for path in [
        "https://evil.invalid/dc/x/",
        "//evil.invalid/dc/x/",
        "/dc/../dls/x/",
        "/dc/x/?token=secret",
        "/dc/%2e%2e/",
        "/dc/x/#secret",
        "/dls/secret",
    ] {
        assert_eq!(
            adapter.detail(path).await.err(),
            Some(ProviderError::InvalidQuery)
        );
    }
    for (query, page) in [("", 1), ("valid", 0), ("valid", 10001), ("bad\nquery", 1)] {
        assert_eq!(
            adapter.search(query, page).await.err(),
            Some(ProviderError::InvalidQuery)
        );
    }
    assert!(mock.state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn detail_keeps_secrets_opaque_and_classifies_mirrors_without_network() {
    let urls = [
        (
            "https://fs3.comicfiles.ru/2026/example.cbz?token=private-token",
            GetComicsLinkState::Direct,
        ),
        (
            "https://pixeldrain.com/u/private-token",
            GetComicsLinkState::ManualAction,
        ),
        (
            "https://1024terabox.com/s/private-token",
            GetComicsLinkState::ManualAction,
        ),
        (
            "https://vikingfile.com/f/private-token",
            GetComicsLinkState::ManualAction,
        ),
        (
            "https://datanodes.to/private-token",
            GetComicsLinkState::ManualAction,
        ),
        (
            "https://fs3.comicfiles.ru.evil.invalid/example.cbz",
            GetComicsLinkState::Unsupported,
        ),
        (
            "https://fs3.comicfiles.ru:8443/example.cbz",
            GetComicsLinkState::Unsupported,
        ),
        (
            "http://fs3.comicfiles.ru/example.cbz",
            GetComicsLinkState::Unsupported,
        ),
        (
            "https://user:password@fs3.comicfiles.ru/example.cbz",
            GetComicsLinkState::Unsupported,
        ),
        (
            "https://fs3.comicfiles.ru/example.cbz#private-token",
            GetComicsLinkState::Unsupported,
        ),
        (
            "https://fs3.comicfiles.ru/view/example",
            GetComicsLinkState::Unsupported,
        ),
        (
            "http://127.0.0.1/private-token",
            GetComicsLinkState::Unsupported,
        ),
        ("javascript:alert(1)", GetComicsLinkState::Unsupported),
        (
            "//evil.invalid/private-token",
            GetComicsLinkState::Unsupported,
        ),
    ];
    let mut urls: Vec<(String, GetComicsLinkState)> = urls
        .into_iter()
        .flat_map(|(url, expected)| {
            let mut cases = vec![(url.to_owned(), expected)];
            if url.contains("fs3.comicfiles.ru") {
                cases.push((
                    url.replace("fs3.comicfiles.ru", "twlv.comicfiles.ru"),
                    expected,
                ));
            }
            cases
        })
        .collect();
    for host in ["fs3.comicfiles.ru", "twlv.comicfiles.ru"] {
        for extension in ["cbz", "cbr", "zip", "rar"] {
            urls.push((
                format!("https://{host}:443/example.{extension}"),
                GetComicsLinkState::Direct,
            ));
        }
        for authority in [format!("sub.{host}"), format!("evil-{host}")] {
            urls.push((
                format!("https://{authority}/example.cbz"),
                GetComicsLinkState::Unsupported,
            ));
        }
    }
    let html = detail(&urls.iter().map(|(url, _)| button(url)).collect::<String>());
    let mock = Mock::start(vec![Reply::html(&html)]).await;
    let adapter = mock.adapter();
    let result = adapter.detail("/dc/example-1/").await.unwrap();
    let public = serde_json::to_string(&result.summary()).unwrap();
    for secret in ["private-token", "password", "https:", "127.0.0.1"] {
        assert!(!public.contains(secret));
    }
    assert_eq!(result.links().len(), urls.len());
    for (link, (_, expected)) in result.links().iter().zip(urls) {
        let outcome = adapter.resolve(link).await.unwrap();
        assert_eq!(outcome.summary.state, expected);
        assert_eq!(
            outcome.into_download().is_some(),
            expected == GetComicsLinkState::Direct
        );
    }
    assert_eq!(mock.state.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn wrappers_use_head_only_and_never_forward_credentials_or_cookies() {
    for host in ["fs3.comicfiles.ru", "twlv.comicfiles.ru"] {
        let mut first = Reply::html(&detail(&button("/dls/synthetic-token")));
        first
            .headers
            .push(("set-cookie", "secret=cookie-value".into()));
        let mock = Mock::start(vec![
            first,
            Reply::redirect("/dls/second-token"),
            Reply::redirect(&format!(
                "https://{host}/2026/synthetic.cbz?token=private-token"
            )),
        ])
        .await;
        let adapter = mock.adapter();
        let result = adapter.detail("/dc/example-1/").await.unwrap();
        assert_eq!(
            result.summary().links[0].state,
            GetComicsLinkState::NeedsResolution
        );
        let resolved = adapter.resolve(&result.links()[0]).await.unwrap();
        assert_eq!(resolved.summary.state, GetComicsLinkState::Direct);
        assert!(resolved.into_download().is_some());
        let requests = mock.state.requests.lock().await;
        assert_eq!(
            requests.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["GET", "HEAD", "HEAD"]
        );
        for (_, uri, headers) in requests.iter() {
            assert!(!uri.contains("private-config-key"));
            for name in ["authorization", "cookie", "referer", "x-api-key"] {
                assert!(!headers.contains_key(name));
            }
        }
    }
}

#[tokio::test]
async fn foreign_redirects_are_classified_without_requesting_destination() {
    let target = Mock::start(vec![]).await;
    let mock = Mock::start(vec![
        Reply::html(&detail(&button("/dls/token"))),
        Reply::redirect(&target.base),
    ])
    .await;
    let adapter = mock.adapter();
    let result = adapter.detail("/dc/example-1/").await.unwrap();
    assert_eq!(
        adapter
            .resolve(&result.links()[0])
            .await
            .unwrap()
            .summary
            .state,
        GetComicsLinkState::Unsupported
    );
    assert_eq!(
        target.adapter().resolve(&result.links()[0]).await.err(),
        Some(ProviderError::Unsupported)
    );
    assert!(target.state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn wrapper_loops_and_redirect_budget_are_bounded() {
    for locations in [
        vec!["/dls/token"],
        vec!["/dls/two", "/dls/three", "/dls/four"],
    ] {
        let count = locations.len();
        let mut replies = vec![Reply::html(&detail(&button("/dls/token")))];
        replies.extend(locations.into_iter().map(Reply::redirect));
        let mock = Mock::start(replies).await;
        let adapter = mock.adapter();
        let result = adapter.detail("/dc/example-1/").await.unwrap();
        assert_eq!(
            adapter
                .resolve(&result.links()[0])
                .await
                .unwrap()
                .summary
                .state,
            GetComicsLinkState::Unsupported
        );
        assert_eq!(mock.state.requests.lock().await.len(), count + 1);
    }
}

#[tokio::test]
async fn challenge_layout_status_and_mime_failures_are_explicit() {
    for (reply, expected) in [
        (
            Reply::html("<form id='challenge-form'></form>"),
            ProviderError::ChallengeRequired,
        ),
        (
            Reply::html("<div class='cf-turnstile'></div>"),
            ProviderError::ChallengeRequired,
        ),
        (
            Reply::html("<html>layout changed</html>"),
            ProviderError::InvalidResponse,
        ),
        (
            Reply {
                status: 503,
                ..Reply::html("offline")
            },
            ProviderError::Unavailable,
        ),
        (
            Reply {
                headers: vec![("content-type", "application/json".into())],
                ..Reply::html(SEARCH)
            },
            ProviderError::InvalidResponse,
        ),
        (
            Reply {
                headers: vec![("cf-mitigated", "challenge".into())],
                ..Reply::html("")
            },
            ProviderError::ChallengeRequired,
        ),
        (
            Reply {
                status: 429,
                headers: vec![("retry-after", "10".into())],
                ..Reply::html("")
            },
            ProviderError::RateLimited {
                retry_after_seconds: Some(10),
            },
        ),
    ] {
        let mock = Mock::start(vec![reply]).await;
        assert_eq!(
            mock.adapter().search("example", 1).await.err(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn response_size_and_timeout_limits_apply() {
    for stall in [false, true] {
        let mock = Mock::start(vec![Reply {
            stall,
            ..Reply::html(&"x".repeat(2048))
        }])
        .await;
        let adapter = mock.with_limits(HttpLimits {
            timeout: Duration::from_millis(100),
            max_body_bytes: 1024,
        });
        assert_eq!(
            adapter.search("example", 1).await.err(),
            Some(if stall {
                ProviderError::Unavailable
            } else {
                ProviderError::InvalidResponse
            })
        );
    }
}

#[tokio::test]
async fn search_rejects_external_posts_and_ignores_external_pagination() {
    let mock = Mock::start(vec![
        Reply::html(&SEARCH.replace(
            "href='/dc/example-1/'",
            "href='https://evil.invalid/dc/example-1/'",
        )),
        Reply::html(&SEARCH.replace("href='/page/2/", "href='https://evil.invalid/page/2/")),
    ])
    .await;
    let adapter = mock.adapter();
    assert_eq!(
        adapter.search("example friends", 1).await.err(),
        Some(ProviderError::InvalidResponse)
    );
    assert_eq!(
        adapter
            .search("example friends", 1)
            .await
            .unwrap()
            .next_page,
        None
    );
}

#[tokio::test]
async fn wrapper_interactive_and_challenge_states_are_distinct() {
    for (reply, expected) in [
        (
            Reply::html("interactive"),
            Ok(GetComicsLinkState::ManualAction),
        ),
        (
            Reply::redirect("https://pixeldrain.com/u/example"),
            Ok(GetComicsLinkState::ManualAction),
        ),
        (
            Reply {
                headers: vec![("cf-mitigated", "challenge".into())],
                ..Reply::html("")
            },
            Err(ProviderError::ChallengeRequired),
        ),
    ] {
        let mock = Mock::start(vec![Reply::html(&detail(&button("/dls/token"))), reply]).await;
        let adapter = mock.adapter();
        let result = adapter.detail("/dc/example-1/").await.unwrap();
        assert_eq!(
            adapter
                .resolve(&result.links()[0])
                .await
                .map(|r| r.summary.state),
            expected
        );
    }
}

#[tokio::test]
async fn source_page_redirects_are_never_followed() {
    let target = Mock::start(vec![]).await;
    let mock = Mock::start(vec![
        Reply::redirect(&target.base),
        Reply::redirect(&target.base),
    ])
    .await;
    let adapter = mock.adapter();
    assert_eq!(
        adapter.search("example", 1).await.err(),
        Some(ProviderError::Unavailable)
    );
    assert_eq!(
        adapter.detail("/dc/example-1/").await.err(),
        Some(ProviderError::Unavailable)
    );
    assert!(target.state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn oversized_result_sets_and_unrecognized_details_fail_explicitly() {
    let posts = (0..101).map(|n| format!("<article><h1 class=post-title><a href='/dc/example-{n}/'>Example</a></h1></article>")).collect::<String>();
    let links = (0..101)
        .map(|n| button(&format!("/dls/synthetic-{n}")))
        .collect::<String>();
    let mock = Mock::start(vec![
        Reply::html(&format!("<div class=post-list-posts>{posts}</div>")),
        Reply::html(&detail(&links)),
        Reply::html(&detail("<a href='/unrelated'>Unrelated</a>")),
        Reply::html("<h1 class=post-title>Example</h1><div class=new-layout></div>"),
    ])
    .await;
    let adapter = mock.adapter();
    assert_eq!(
        adapter.search("example", 1).await.err(),
        Some(ProviderError::InvalidResponse)
    );
    assert_eq!(
        adapter.detail("/dc/example-1/").await.err(),
        Some(ProviderError::InvalidResponse)
    );
    assert_eq!(
        adapter.detail("/dc/example-1/").await.err(),
        Some(ProviderError::Unsupported)
    );
    assert_eq!(
        adapter.detail("/dc/example-1/").await.err(),
        Some(ProviderError::InvalidResponse)
    );
}

#[tokio::test]
async fn title_cannot_reflect_config_credentials_or_full_download_urls() {
    for title in [
        "private-config-key",
        "https://example.invalid/file?token=private-token",
    ] {
        let html = detail(&button("/dls/synthetic")).replace("Example #1", title);
        let mock = Mock::start(vec![Reply::html(&html)]).await;
        assert_eq!(
            mock.adapter().detail("/dc/example-1/").await.err(),
            Some(ProviderError::InvalidResponse)
        );
    }
}
