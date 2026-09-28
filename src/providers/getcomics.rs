//! GetComics HTML and HEAD-only link adapter; never downloads archive payloads.
use super::{ProviderConfig, ProviderError, http::Http, query_valid};
use reqwest::{Client, Url, header};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use std::{collections::HashSet, time::Duration};

const MAX_POSTS: usize = 100;
const MAX_LINKS: usize = 100;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_WRAPPERS: usize = 3;

pub struct GetComicsAdapter {
    http: Http,
    origin: Url,
    resolver: Client,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GetComicsPost {
    pub post_path: String,
    pub title: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GetComicsSearchPage {
    pub posts: Vec<GetComicsPost>,
    pub page: u32,
    pub next_page: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GetComicsLinkState {
    Direct,
    NeedsResolution,
    ManualAction,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GetComicsLinkSummary {
    pub state: GetComicsLinkState,
    /// Fixed labels, never reflected hostnames or link text from the page.
    pub host: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GetComicsDetailSummary {
    pub post: GetComicsPost,
    pub links: Vec<GetComicsLinkSummary>,
}

/// Opaque server-side selection. Intentionally no Debug, Serialize, or URL accessor.
pub struct GetComicsLink {
    origin: Url,
    target: Option<Url>,
    summary: GetComicsLinkSummary,
}

impl GetComicsLink {
    /// Re-identify a selection without persisting its credential-bearing target.
    pub(crate) fn identity_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let value = self
            .target
            .as_ref()
            .map(Url::as_str)
            .unwrap_or("unsupported");
        Sha256::digest(value.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

pub struct GetComicsDetail {
    post: GetComicsPost,
    links: Vec<GetComicsLink>,
}

impl GetComicsDetail {
    pub fn summary(&self) -> GetComicsDetailSummary {
        GetComicsDetailSummary {
            post: self.post.clone(),
            links: self.links.iter().map(|l| l.summary.clone()).collect(),
        }
    }

    pub fn links(&self) -> &[GetComicsLink] {
        &self.links
    }
}

/// Ephemeral download capability, never a public DTO.
pub struct GetComicsDownload {
    url: Url,
}

impl GetComicsDownload {
    /// The acquisition worker must independently enforce its download network policy.
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }
}

pub struct GetComicsResolution {
    pub summary: GetComicsLinkSummary,
    download: Option<GetComicsDownload>,
}

impl GetComicsResolution {
    pub fn into_download(self) -> Option<GetComicsDownload> {
        self.download
    }
}

impl GetComicsAdapter {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        let http = Http::new(config, false)?;
        let origin = http.download_target(".")?;
        if origin.path() != "/" {
            return Err(ProviderError::InvalidConfiguration);
        }
        let resolver = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent("libraryd/0.1")
            .connect_timeout(Duration::from_secs(5))
            .timeout(RESOLVE_TIMEOUT)
            .build()
            .map_err(|_| ProviderError::InvalidConfiguration)?;
        Ok(Self {
            http,
            origin,
            resolver,
        })
    }

    /// One-based site pagination, bounded to 10,000 pages and 100 posts per response.
    pub async fn search(
        &self,
        query: &str,
        page: u32,
    ) -> Result<GetComicsSearchPage, ProviderError> {
        query_valid(query)?;
        if !(1..=10_000).contains(&page) {
            return Err(ProviderError::InvalidQuery);
        }
        let path = if page == 1 {
            String::new()
        } else {
            format!("page/{page}/")
        };
        let request = self.http.get(&path)?.query(&[("s", query.trim())]);
        let document = self.html(request).await?;
        let list = document.select(&selector(".post-list-posts")).next();
        if list.is_none()
            && document
                .select(&selector(".post-list .pagination-noresults"))
                .next()
                .is_some()
        {
            return Ok(GetComicsSearchPage {
                posts: Vec::new(),
                page,
                next_page: None,
            });
        }
        let list = list.ok_or(ProviderError::InvalidResponse)?;
        let mut posts = Vec::new();
        let mut seen = HashSet::new();
        for article in list.select(&selector("article")) {
            let Some(link) = article.select(&selector(".post-title a[href]")).next() else {
                return Err(ProviderError::InvalidResponse);
            };
            let raw = link.value().attr("href").unwrap_or_default();
            let url = self
                .origin
                .join(raw)
                .map_err(|_| ProviderError::InvalidResponse)?;
            if !safe_url_text(raw)
                || url.origin() != self.origin.origin()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ProviderError::InvalidResponse);
            }
            // Exclude news/sponsored posts without treating them as acquisitions.
            if !post_path_valid(url.path()) {
                continue;
            }
            let title = self.title(link)?;
            if seen.insert(url.path().to_owned()) {
                posts.push(GetComicsPost {
                    post_path: url.path().to_owned(),
                    title,
                });
                if posts.len() > MAX_POSTS {
                    return Err(ProviderError::InvalidResponse);
                }
            }
        }
        let next_path = format!("/page/{}/", page + 1);
        let has_next = document.select(&selector(".pagination a[href]")).any(|a| {
            a.value()
                .attr("href")
                .and_then(|s| self.origin.join(s).ok())
                .is_some_and(|url| {
                    url.origin() == self.origin.origin()
                        && url.path() == next_path
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                        && url.query_pairs().eq([("s".into(), query.trim().into())])
                })
        });
        Ok(GetComicsSearchPage {
            posts,
            page,
            next_page: (has_next && page < 10_000).then_some(page + 1),
        })
    }

    pub async fn detail(&self, post_path: &str) -> Result<GetComicsDetail, ProviderError> {
        if !post_path_valid(post_path) {
            return Err(ProviderError::InvalidQuery);
        }
        let document = self.html(self.http.get(post_path)?).await?;
        let heading = document
            .select(&selector("h1.post-title"))
            .next()
            .ok_or(ProviderError::InvalidResponse)?;
        let content = document
            .select(&selector("article.post-body .post-contents"))
            .next()
            .ok_or(ProviderError::InvalidResponse)?;
        let mut links = Vec::new();
        let mut seen = HashSet::new();
        for anchor in content.select(&selector("a[href]")) {
            if !anchor.value().classes().any(|c| c.starts_with("aio-")) {
                continue;
            }
            let raw = anchor.value().attr("href").unwrap_or_default();
            let target = safe_url_text(raw)
                .then(|| self.origin.join(raw).ok())
                .flatten();
            let summary = target
                .as_ref()
                .map(|u| self.classify(u))
                .unwrap_or_else(unsupported);
            // Deduplicate opaque targets without publishing them as IDs.
            if !seen.insert(raw.to_owned()) {
                continue;
            }
            links.push(GetComicsLink {
                origin: self.origin.clone(),
                target,
                summary,
            });
            if links.len() > MAX_LINKS {
                return Err(ProviderError::InvalidResponse);
            }
        }
        if links.is_empty() {
            return Err(ProviderError::Unsupported);
        }
        Ok(GetComicsDetail {
            post: GetComicsPost {
                post_path: post_path.to_owned(),
                title: self.title(heading)?,
            },
            links,
        })
    }

    /// Only same-origin wrappers are requested, using HEAD. Mirror URLs are not fetched.
    pub async fn resolve(
        &self,
        link: &GetComicsLink,
    ) -> Result<GetComicsResolution, ProviderError> {
        if link.origin != self.origin {
            return Err(ProviderError::Unsupported);
        }
        let Some(target) = &link.target else {
            return Ok(resolution(unsupported(), None));
        };
        tokio::time::timeout(RESOLVE_TIMEOUT, async {
            let mut target = target.clone();
            let mut seen = HashSet::new();
            let mut requests = 0;
            loop {
                let summary = self.classify(&target);
                if summary.state != GetComicsLinkState::NeedsResolution {
                    let download = (summary.state == GetComicsLinkState::Direct)
                        .then_some(GetComicsDownload { url: target });
                    return Ok(resolution(summary, download));
                }
                if requests == MAX_WRAPPERS || !seen.insert(target.clone()) {
                    return Ok(resolution(unsupported(), None));
                }
                requests += 1;
                let response = self
                    .resolver
                    .head(target.clone())
                    .send()
                    .await
                    .map_err(|_| ProviderError::Unavailable)?;
                if response
                    .headers()
                    .get("cf-mitigated")
                    .is_some_and(|v| v == "challenge")
                {
                    return Err(ProviderError::ChallengeRequired);
                }
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(ProviderError::RateLimited {
                        retry_after_seconds: response
                            .headers()
                            .get(header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse().ok()),
                    });
                }
                if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                    let raw = response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .ok_or(ProviderError::InvalidResponse)?;
                    if !safe_url_text(raw) {
                        return Ok(resolution(unsupported(), None));
                    }
                    target = target
                        .join(raw)
                        .map_err(|_| ProviderError::InvalidResponse)?;
                } else if response.status().is_success()
                    || matches!(response.status().as_u16(), 401 | 403 | 405)
                {
                    return Ok(resolution(
                        GetComicsLinkSummary {
                            state: GetComicsLinkState::ManualAction,
                            host: "getcomics",
                        },
                        None,
                    ));
                } else {
                    return Err(ProviderError::Unavailable);
                }
            }
        })
        .await
        .map_err(|_| ProviderError::Unavailable)?
    }

    async fn html(&self, request: reqwest::RequestBuilder) -> Result<Html, ProviderError> {
        let response = self
            .http
            .execute(request.header(header::ACCEPT, "text/html"))
            .await?;
        let text =
            std::str::from_utf8(&response.body).map_err(|_| ProviderError::InvalidResponse)?;
        let lower = text.to_ascii_lowercase();
        let document = Html::parse_document(text);
        if response.challenge || document.select(&selector(
            "#challenge-form, .cf-turnstile, .g-recaptcha, .h-captcha, [name=cf-turnstile-response], [name=g-recaptcha-response], [name=h-captcha-response]"
        )).next().is_some() || (lower.contains("_cf_chl_opt") && lower.contains("/challenge-platform/")) {
            return Err(ProviderError::ChallengeRequired);
        }
        if !response.status.is_success() {
            return Err(ProviderError::Unavailable);
        }
        if !response
            .content_type
            .split(';')
            .next()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/html"))
        {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(document)
    }

    fn title(&self, element: ElementRef<'_>) -> Result<String, ProviderError> {
        let text = element.text().collect::<String>();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.contains("://") || text.contains("www.") {
            return Err(ProviderError::InvalidResponse);
        }
        self.http.text(&text)
    }

    fn classify(&self, url: &Url) -> GetComicsLinkSummary {
        if !safe_url_text(url.as_str())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return unsupported();
        }
        if url.origin() == self.origin.origin()
            && url.path().starts_with("/dls/")
            && url.path().len() > 5
            && url.query().is_none()
        {
            return GetComicsLinkSummary {
                state: GetComicsLinkState::NeedsResolution,
                host: "getcomics",
            };
        }
        if url.scheme() != "https" || url.port_or_known_default() != Some(443) {
            return unsupported();
        }
        let host = match url.host_str().unwrap_or_default() {
            host if direct_host_allowed(host) => {
                let path = url.path().to_ascii_lowercase();
                let archive = [".cbz", ".cbr", ".zip", ".rar"]
                    .iter()
                    .any(|ext| path.ends_with(ext));
                return GetComicsLinkSummary {
                    state: if archive {
                        GetComicsLinkState::Direct
                    } else {
                        GetComicsLinkState::Unsupported
                    },
                    host: "comicfiles",
                };
            }
            "pixeldrain.com" => "pixeldrain",
            "1024terabox.com" | "terabox.com" | "www.terabox.com" => "terabox",
            "vikingfile.com" => "vikingfile",
            "datanodes.to" => "datanodes",
            "mega.nz" | "mega.co.nz" => "mega",
            "mediafire.com" | "www.mediafire.com" => "mediafire",
            _ => return unsupported(),
        };
        GetComicsLinkSummary {
            state: GetComicsLinkState::ManualAction,
            host,
        }
    }
}

pub(crate) fn direct_host_allowed(host: &str) -> bool {
    matches!(host, "fs3.comicfiles.ru" | "twlv.comicfiles.ru")
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static GetComics selector")
}

fn safe_url_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && !value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
}

fn post_path_valid(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    value.len() <= 512
        && parts.len() == 4
        && parts[0].is_empty()
        && parts[3].is_empty()
        && matches!(parts[1], "dc" | "marvel" | "other-comics")
        && !parts[2].is_empty()
        && parts[2]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn unsupported() -> GetComicsLinkSummary {
    GetComicsLinkSummary {
        state: GetComicsLinkState::Unsupported,
        host: "unsupported",
    }
}

fn resolution(
    summary: GetComicsLinkSummary,
    download: Option<GetComicsDownload>,
) -> GetComicsResolution {
    GetComicsResolution { summary, download }
}
