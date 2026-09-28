use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response},
};
use libraryd::providers::mangadex_chapters::*;
use libraryd::providers::{HttpLimits, ProviderConfig, ProviderError, SearchPage};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;

const MANGA: &str = "11111111-1111-4111-8111-111111111111";
const CHAPTER: &str = "22222222-2222-4222-8222-222222222222";
const GROUP: &str = "33333333-3333-4333-8333-333333333333";
const HASH: &str = "0123456789abcdef0123456789abcdef";
const BASE: &str = "https://node.mangadex.network/temporary-secret";

struct Reply {
    status: u16,
    body: String,
    location: Option<String>,
}
impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            location: None,
        }
    }
}
#[derive(Clone)]
struct MockState {
    apply_chapter_filters: bool,
    replies: Arc<Mutex<VecDeque<Reply>>>,
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
    async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with_filters(replies, false).await
    }
    async fn start_with_filters(replies: Vec<Reply>, apply_chapter_filters: bool) -> Self {
        let state = MockState {
            apply_chapter_filters,
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handler).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
    fn provider(&self) -> MangaDexChapters {
        MangaDexChapters::new(ProviderConfig::new(&self.base, None, HttpLimits::default()).unwrap())
            .unwrap()
    }
}
async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    assert_eq!(request.method(), "GET");
    assert_eq!(request.headers()["accept"], "application/json");
    state.requests.lock().await.push(request.uri().to_string());
    let mut reply = state
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected HTTP request");
    if state.apply_chapter_filters && request.uri().path() == "/prefix/chapter" {
        let uri = request.uri().to_string();
        let mut body: Value = serde_json::from_str(&reply.body).unwrap();
        let data = body["data"].as_array_mut().unwrap();
        // MangaDex's include flags are tri-state predicates: absent / required / excluded.
        for (key, field) in [
            ("includeExternalUrl", "externalUrl"),
            ("includeEmptyPages", "pages"),
        ] {
            if let Some(filter) = param(&uri, key) {
                assert!(matches!(filter.as_str(), "0" | "1"));
                data.retain(|chapter| {
                    let property = if field == "pages" {
                        chapter["attributes"][field] == 0
                    } else {
                        !chapter["attributes"][field].is_null()
                    };
                    property == (filter == "1")
                });
            }
        }
        let total = data.len();
        body["total"] = json!(total);
        reply.body = body.to_string();
    }
    let mut response = Response::builder()
        .status(reply.status)
        .header("content-type", "application/json");
    if let Some(location) = reply.location {
        response = response.header("location", location);
    }
    response.body(Body::from(reply.body)).unwrap()
}
fn param(uri: &str, name: &str) -> Option<String> {
    reqwest::Url::parse(&format!("http://example.invalid{uri}"))
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}
fn item(language: &str) -> Value {
    json!({"id":CHAPTER,"type":"chapter","attributes":{
        "title":"A fractional chapter", "chapter":"10.5", "volume":"01.0",
        "translatedLanguage":language,"pages":2,"version":3,"externalUrl":null,
        "isUnavailable":false
    },"relationships":[{"id":MANGA,"type":"manga"},
        {"id":GROUP,"type":"scanlation_group","attributes":{"name":"Mock group"}}]})
}
fn listing(data: Vec<Value>, limit: u32, offset: u32, total: u64) -> Value {
    json!({"result":"ok","response":"collection","data":data,"limit":limit,"offset":offset,"total":total})
}
fn entity(data: Value) -> Value {
    json!({"result":"ok","response":"entity","data":data})
}

#[tokio::test]
async fn get_chapter_preserves_listing_metadata_without_requesting_images() {
    for language in ["en", "pt-br", "zh-hk"] {
        let mut value = item(language);
        value["attributes"]["externalUrl"] = json!("https://publisher.example/chapter/1");
        value["relationships"].as_array_mut().unwrap().push(json!({
            "id":"abcdefab-abcd-4abc-8abc-abcdefabcdef", "type":"scanlation_group",
            "attributes":{"name":"Second group"}
        }));
        let mock = Mock::start(vec![
            Reply::json(listing(vec![value.clone()], 20, 0, 1)),
            Reply::json(entity(value)),
        ])
        .await;
        let provider = mock.provider();
        let listed = provider
            .list_chapters(MANGA, language, SearchPage::default())
            .await
            .unwrap();
        let fetched = provider.get_chapter(CHAPTER).await.unwrap();
        assert_eq!(fetched, listed.chapters[0]);
        assert_eq!(fetched.language, language);
        assert_eq!(fetched.scanlation_groups.len(), 2);
        assert_eq!(fetched.scanlation_groups[0].name, "Mock group");
        assert_eq!(fetched.scanlation_groups[1].name, "Second group");
        let requests = mock.state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with(&format!("/prefix/chapter/{CHAPTER}?")));
        assert_eq!(
            param(&requests[1], "includes[]").as_deref(),
            Some("scanlation_group")
        );
        assert!(mock.state.replies.lock().await.is_empty());
    }
}

#[tokio::test]
async fn get_chapter_rejects_invalid_input_and_wrong_entity_identity() {
    let mock = Mock::start(vec![]).await;
    for id in [
        "bad",
        "../chapter",
        "00000000-0000-0000-0000-000000000000",
        "22222222222242228222222222222222",
    ] {
        assert_eq!(
            mock.provider().get_chapter(id).await.unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
    assert!(mock.state.requests.lock().await.is_empty());
    let mut wrong_id = item("en");
    wrong_id["id"] = json!(GROUP);
    let mut wrong_type = item("en");
    wrong_type["type"] = json!("manga");
    for body in [
        entity(wrong_id),
        entity(wrong_type),
        entity(Value::Null),
        listing(vec![item("en")], 20, 0, 1),
        json!({"result":"error","response":"entity","data":item("en")}),
        json!({"result":"ok","data":item("en")}),
    ] {
        let mock = Mock::start(vec![Reply::json(body)]).await;
        assert_eq!(
            mock.provider().get_chapter(CHAPTER).await.unwrap_err(),
            ProviderError::InvalidResponse
        );
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
    let mock = Mock::start(vec![Reply {
        status: 200,
        body: "{broken".into(),
        location: None,
    }])
    .await;
    assert_eq!(
        mock.provider().get_chapter(CHAPTER).await.unwrap_err(),
        ProviderError::InvalidResponse
    );
}

#[tokio::test]
async fn get_and_list_share_chapter_validation() {
    for (pointer, invalid) in [
        ("/id", json!("invalid")),
        ("/attributes/translatedLanguage", json!("EN")),
        ("/attributes/pages", json!(1001)),
        ("/attributes/pages", json!(-1)),
        ("/attributes/pages", Value::Null),
        ("/attributes/version", json!(0)),
        ("/attributes/chapter", json!("123456789")),
        ("/attributes/volume", json!("v".repeat(65))),
        ("/attributes/title", json!("bad\ntitle")),
        ("/attributes/title", json!("t".repeat(256))),
        ("/attributes/externalUrl", json!("javascript:alert(1)")),
        (
            "/attributes/externalUrl",
            json!("https://user:pass@publisher.example/chapter"),
        ),
        (
            "/attributes/externalUrl",
            json!("https://publisher.example/?token=secret"),
        ),
        (
            "/attributes/externalUrl",
            json!(format!("https://publisher.example/{}", "p".repeat(512))),
        ),
        ("/relationships/0/id", json!("bad")),
        ("/relationships/1/id", json!("bad")),
        ("/relationships/1/attributes", Value::Null),
        ("/relationships/1/attributes/name", json!("")),
        ("/relationships/1/attributes/name", json!("bad\nname")),
        ("/relationships", json!([])),
    ] {
        let mut value = item("en");
        *value.pointer_mut(pointer).unwrap() = invalid;
        let mock = Mock::start(vec![
            Reply::json(entity(value.clone())),
            Reply::json(listing(vec![value], 20, 0, 1)),
        ])
        .await;
        let provider = mock.provider();
        assert_eq!(
            provider.get_chapter(CHAPTER).await.unwrap_err(),
            ProviderError::InvalidResponse,
            "{pointer}"
        );
        assert_eq!(
            provider
                .list_chapters(MANGA, "en", SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse,
            "{pointer}"
        );
    }
    for index in [0, 1] {
        let mut value = item("en");
        let duplicate = value["relationships"][index].clone();
        value["relationships"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        let mock = Mock::start(vec![Reply::json(entity(value))]).await;
        assert_eq!(
            mock.provider().get_chapter(CHAPTER).await.unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
}
fn manifest() -> Value {
    json!({"result":"ok","baseUrl":BASE,"chapter":{"hash":HASH,
        "data":["1-a.png","2-b.webp"],"dataSaver":["1-c.jpg","2-d.jpg"]}})
}
fn chapter() -> MangaDexChapter {
    MangaDexChapter {
        id: CHAPTER.into(),
        manga_id: MANGA.into(),
        language: "en".into(),
        chapter: Some("10.5".into()),
        volume: None,
        title: None,
        scanlation_groups: vec![],
        external_url: None,
        page_count: 2,
        version: 3,
        is_unavailable: false,
    }
}

#[tokio::test]
async fn chapter_listing_preserves_labels_groups_languages_and_pages() {
    for language in ["en", "pt-br", "zh-hk"] {
        let mut second = item(language);
        second["id"] = json!(GROUP);
        second["attributes"]["chapter"] = Value::Null;
        second["attributes"]["title"] = json!("");
        second["relationships"].as_array_mut().unwrap().pop();
        let mock = Mock::start(vec![
            Reply::json(listing(vec![item(language)], 1, 0, 2)),
            Reply::json(listing(vec![second], 1, 1, 2)),
        ])
        .await;
        let provider = mock.provider();
        let first = provider
            .list_chapters(MANGA, language, SearchPage { number: 1, size: 1 })
            .await
            .unwrap();
        let value = &first.chapters[0];
        assert_eq!(value.id, CHAPTER);
        assert_eq!(value.manga_id, MANGA);
        assert_eq!(value.language, language);
        assert_eq!(value.chapter.as_deref(), Some("10.5"));
        assert_eq!(value.volume.as_deref(), Some("01.0"));
        assert_eq!(value.title.as_deref(), Some("A fractional chapter"));
        assert_eq!(value.scanlation_groups[0].id, GROUP);
        assert_eq!(value.scanlation_groups[0].name, "Mock group");
        assert_eq!((value.page_count, value.version), (2, 3));
        assert_eq!(first.next_page, Some(2));
        let second = provider
            .list_chapters(MANGA, language, SearchPage { number: 2, size: 1 })
            .await
            .unwrap();
        assert_eq!(second.next_page, None);
        assert_eq!(second.chapters[0].chapter, None);
        assert_eq!(second.chapters[0].title.as_deref(), Some(""));
        assert!(second.chapters[0].scanlation_groups.is_empty());
        let requests = mock.state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("/prefix/chapter?"));
        for (key, value) in [
            ("manga", MANGA),
            ("translatedLanguage[]", language),
            ("includes[]", "scanlation_group"),
            ("limit", "1"),
            ("offset", "0"),
            ("includeFutureUpdates", "0"),
            ("includeFuturePublishAt", "0"),
            ("includeUnavailable", "1"),
        ] {
            assert_eq!(param(&requests[0], key).as_deref(), Some(value));
        }
        for request in requests.iter() {
            assert_eq!(param(request, "includeExternalUrl"), None);
            assert_eq!(param(request, "includeEmptyPages"), None);
        }
        assert_eq!(param(&requests[1], "offset").as_deref(), Some("1"));
        assert!(mock.state.replies.lock().await.is_empty());
    }
}

#[tokio::test]
async fn unfiltered_listing_keeps_hosted_external_and_empty_chapters() {
    let mut external = item("en");
    external["id"] = json!(GROUP);
    external["attributes"]["externalUrl"] = json!("https://publisher.example/chapter/1");
    let mut empty = item("en");
    empty["id"] = json!("44444444-4444-4444-8444-444444444444");
    empty["attributes"]["pages"] = json!(0);
    let mock = Mock::start_with_filters(
        vec![
            Reply::json(listing(vec![item("en"), external, empty], 20, 0, 3)),
            Reply::json(manifest()),
        ],
        true,
    )
    .await;
    let provider = mock.provider();
    let page = provider
        .list_chapters(MANGA, "en", SearchPage::default())
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.next_page, None);
    assert_eq!(page.chapters.len(), 3);
    let hosted = &page.chapters[0];
    assert_eq!(hosted.id, CHAPTER);
    assert_eq!(hosted.page_count, 2);
    assert!(hosted.external_url.is_none());
    assert!(page.chapters[1].external_url.is_some());
    assert_eq!(page.chapters[2].page_count, 0);
    for chapter in &page.chapters[1..] {
        assert_eq!(
            provider.at_home(chapter).await.unwrap_err(),
            ProviderError::Unsupported
        );
    }
    assert_eq!(mock.state.requests.lock().await.len(), 1);
    let manifest = provider.at_home(hosted).await.unwrap();
    assert_eq!(manifest.summary().chapter_id, CHAPTER);
    assert_eq!(manifest.summary().page_count, 2);
    assert_eq!(mock.state.requests.lock().await.len(), 2);
    assert!(mock.state.replies.lock().await.is_empty());
}

#[tokio::test]
async fn chapter_listing_rejects_wrong_identity_language_and_malformed_records() {
    for field in [
        "manga",
        "language",
        "id",
        "type",
        "pages",
        "version",
        "group",
        "group_name",
        "missing_pages",
        "relationships",
    ] {
        let mut value = item("en");
        match field {
            "manga" => value["relationships"][0]["id"] = json!(GROUP),
            "language" => value["attributes"]["translatedLanguage"] = json!("ja"),
            "id" => value["id"] = json!("../chapter"),
            "type" => value["type"] = json!("manga"),
            "pages" => value["attributes"]["pages"] = json!(1001),
            "version" => value["attributes"]["version"] = json!(0),
            "group" => value["relationships"][1]["id"] = json!("bad"),
            "group_name" => value["relationships"][1]["attributes"] = Value::Null,
            "missing_pages" => {
                value["attributes"].as_object_mut().unwrap().remove("pages");
            }
            "relationships" => value["relationships"] = json!([]),
            _ => unreachable!(),
        }
        let mock = Mock::start(vec![Reply::json(listing(vec![value], 20, 0, 1))]).await;
        assert_eq!(
            mock.provider()
                .list_chapters(MANGA, "en", SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse,
            "{field}"
        );
    }
    let mut duplicate = item("en");
    duplicate["id"] = json!(CHAPTER.to_uppercase());
    for body in [
        listing(vec![item("en"), duplicate], 20, 0, 2),
        listing(vec![item("en")], 20, 1, 1),
        listing(vec![item("en")], 10, 0, 1),
        listing(vec![], 20, 0, 1),
        listing(vec![item("en")], 20, 0, 0),
        json!({"result":"error"}),
    ] {
        let mock = Mock::start(vec![Reply::json(body)]).await;
        assert_eq!(
            mock.provider()
                .list_chapters(MANGA, "en", SearchPage::default())
                .await
                .unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
}

#[tokio::test]
async fn query_and_pagination_bounds_prevent_requests_and_stop_at_cap() {
    let mock = Mock::start(vec![]).await;
    for (id, language, page) in [
        ("bad", "en", SearchPage::default()),
        (MANGA, "", SearchPage::default()),
        (MANGA, "en&x=y", SearchPage::default()),
        (MANGA, "EN", SearchPage::default()),
        (
            MANGA,
            "en",
            SearchPage {
                number: 0,
                size: 20,
            },
        ),
        (
            MANGA,
            "en",
            SearchPage {
                number: 1,
                size: 101,
            },
        ),
        (
            MANGA,
            "en",
            SearchPage {
                number: 101,
                size: 100,
            },
        ),
        (
            MANGA,
            "en",
            SearchPage {
                number: u32::MAX,
                size: 100,
            },
        ),
    ] {
        assert_eq!(
            mock.provider()
                .list_chapters(id, language, page)
                .await
                .unwrap_err(),
            ProviderError::InvalidQuery
        );
    }
    assert!(mock.state.requests.lock().await.is_empty());
    let mock = Mock::start(vec![Reply::json(listing(vec![item("en")], 1, 9999, 10001))]).await;
    assert_eq!(
        mock.provider()
            .list_chapters(
                MANGA,
                "en",
                SearchPage {
                    number: 10000,
                    size: 1
                }
            )
            .await
            .unwrap()
            .next_page,
        None
    );
}

#[tokio::test]
async fn external_unavailable_and_zero_page_chapters_cannot_request_manifest() {
    for kind in ["external", "empty", "unavailable"] {
        let mut value = item("en");
        match kind {
            "external" => {
                value["attributes"]["externalUrl"] = json!("https://publisher.example/chapter/1")
            }
            "empty" => value["attributes"]["pages"] = json!(0),
            "unavailable" => value["attributes"]["isUnavailable"] = json!(true),
            _ => unreachable!(),
        }
        let mock = Mock::start(vec![Reply::json(listing(vec![value], 20, 0, 1))]).await;
        let provider = mock.provider();
        let page = provider
            .list_chapters(MANGA, "en", SearchPage::default())
            .await
            .unwrap();
        if kind == "external" {
            assert!(page.chapters[0].external_url.is_some());
        }
        assert_eq!(
            provider.at_home(&page.chapters[0]).await.unwrap_err(),
            ProviderError::Unsupported
        );
        assert_eq!(mock.state.requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn opaque_manifest_summary_and_identity_exclude_temporary_authority() {
    let mut moved = manifest();
    moved["baseUrl"] = json!("https://other.mangadex.network:443/other-secret/");
    let mut reordered = manifest();
    reordered["chapter"]["data"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let mut changed_hash = manifest();
    changed_hash["chapter"]["hash"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let mock = Mock::start(vec![
        Reply::json(manifest()),
        Reply::json(moved),
        Reply::json(reordered),
        Reply::json(changed_hash),
    ])
    .await;
    let provider = mock.provider();
    let first = provider.at_home(&chapter()).await.unwrap();
    let second = provider.at_home(&chapter()).await.unwrap();
    let third = provider.at_home(&chapter()).await.unwrap();
    let fourth = provider.at_home(&chapter()).await.unwrap();
    assert_eq!(first.identity(), second.identity());
    assert_ne!(first.identity(), third.identity());
    assert_ne!(first.identity(), fourth.identity());
    assert_eq!(first.identity().len(), 64);
    assert_eq!(
        serde_json::to_value(first.summary()).unwrap(),
        json!({"chapter_id":CHAPTER,"hash":HASH,"page_count":2})
    );
    for private in ["temporary-secret", "mangadex.network", "1-a.png", "baseUrl"] {
        assert!(!format!("{first:?}").contains(private));
        assert!(
            !serde_json::to_string(first.summary())
                .unwrap()
                .contains(private)
        );
    }
    let requests = mock.state.requests.lock().await;
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert!(request.starts_with(&format!("/prefix/at-home/server/{CHAPTER}?")));
        assert_eq!(param(request, "forcePort443").as_deref(), Some("true"));
    }
}

#[tokio::test]
async fn manifest_rejects_bad_hosts_credentials_and_url_components() {
    for base in [
        "http://node.mangadex.network/token",
        "https://127.0.0.1/token",
        "https://2130706433/token",
        "https://[::1]/token",
        "https://10.0.0.1/token",
        "https://localhost/token",
        "https://node.local/token",
        "https://node.internal/token",
        "https://router.home.arpa/token",
        "https://node.mangadex.network:444/token",
        "https://user:pass@node.mangadex.network/token",
        "https://@node.mangadex.network/token",
        "https://node.mangadex.network/token?secret=1",
        "https://node.mangadex.network/token#fragment",
        "https://node.mangadex.network./token",
        "https://bad_host.mangadex.network/token",
        "https://node.mangadex.network/\nsecret",
        "https://node.mangadex.network\\evil/token",
    ] {
        let mut body = manifest();
        body["baseUrl"] = json!(base);
        let mock = Mock::start(vec![Reply::json(body)]).await;
        assert_eq!(
            mock.provider().at_home(&chapter()).await.unwrap_err(),
            ProviderError::InvalidResponse,
            "{base}"
        );
    }
}

#[tokio::test]
async fn manifest_rejects_missing_pages_duplicates_and_path_traversal() {
    for list in ["data", "dataSaver"] {
        for files in [
            json!([]),
            json!(["1.png"]),
            json!(["1.png", "2.png", "3.png"]),
            json!(["1.png", "1.png"]),
            json!(["a.PNG", "A.png"]),
            json!(["../1.png", "2.jpg"]),
            json!(["%2e%2e%2f1.png", "2.jpg"]),
            json!(["/1.png", "2.jpg"]),
            json!(["dir\\1.png", "2.jpg"]),
            json!(["https://evil.com/1.png", "2.jpg"]),
            json!(["1.png?token=x", "2.jpg"]),
            json!(["1.svg", "2.jpg"]),
            json!([".png", "2.jpg"]),
            json!(["1.png#x", "2.jpg"]),
        ] {
            let mut body = manifest();
            body["chapter"][list] = files;
            let mock = Mock::start(vec![Reply::json(body)]).await;
            assert_eq!(
                mock.provider().at_home(&chapter()).await.unwrap_err(),
                ProviderError::InvalidResponse
            );
        }
        let mut body = manifest();
        body["chapter"].as_object_mut().unwrap().remove(list);
        let mock = Mock::start(vec![Reply::json(body)]).await;
        assert_eq!(
            mock.provider().at_home(&chapter()).await.unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
    for hash in ["../bad", "", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
        let mut body = manifest();
        body["chapter"]["hash"] = json!(hash);
        let mock = Mock::start(vec![Reply::json(body)]).await;
        assert_eq!(
            mock.provider().at_home(&chapter()).await.unwrap_err(),
            ProviderError::InvalidResponse
        );
    }
}

#[tokio::test]
async fn shared_http_refuses_redirects_and_bounds_bodies_for_both_endpoints() {
    for at_home in [false, true] {
        let target = Mock::start(vec![]).await;
        let mock = Mock::start(vec![Reply {
            status: 302,
            body: String::new(),
            location: Some(target.base.clone()),
        }])
        .await;
        let provider = mock.provider();
        let error = if at_home {
            provider.at_home(&chapter()).await.unwrap_err()
        } else {
            provider
                .list_chapters(MANGA, "en", SearchPage::default())
                .await
                .unwrap_err()
        };
        assert_eq!(error, ProviderError::Unavailable);
        assert!(target.state.requests.lock().await.is_empty());
        let mock = Mock::start(vec![Reply::json(manifest())]).await;
        let provider = MangaDexChapters::new(
            ProviderConfig::new(
                &mock.base,
                None,
                HttpLimits {
                    max_body_bytes: 32,
                    ..HttpLimits::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
        let error = if at_home {
            provider.at_home(&chapter()).await.unwrap_err()
        } else {
            provider
                .list_chapters(MANGA, "en", SearchPage::default())
                .await
                .unwrap_err()
        };
        assert_eq!(error, ProviderError::InvalidResponse);
    }
}
