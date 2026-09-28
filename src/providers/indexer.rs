use super::{ContentType, ProviderConfig, ProviderError, SearchPage, http::Http, query_valid};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseProtocol {
    Torrent,
    Usenet,
}

/// Administrator-selected categories, verified against the indexer's caps.
/// Manga has no universally distinct Newznab category; configure each indexer.
#[derive(Clone, Debug)]
pub struct CategoryMap {
    pub comics: Vec<u32>,
    pub manga: Vec<u32>,
    pub magazines: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IndexerCapabilities {
    pub generic_search: bool,
    pub categories: Vec<u32>,
    pub max_limit: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseCandidate {
    pub indexer_id: u32,
    pub guid: String,
    pub title: String,
    pub content_type: ContentType,
    pub categories: Vec<u32>,
    pub size_bytes: u64,
    pub protocol: ReleaseProtocol,
    /// Provider RSS publication date; this is not the publication's cover date.
    pub published_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleasePage {
    pub releases: Vec<ReleaseCandidate>,
    pub offset: u32,
    pub total: Option<u64>,
    pub next_offset: Option<u32>,
}

/// Server-side credentialed target. Deliberately neither Debug nor Serialize.
/// Only this module can construct it; retrieval is bound to a configured origin.
pub struct ReleaseDownload {
    url: reqwest::Url,
    indexer_id: u32,
}

pub struct AcquisitionRelease {
    pub release: ReleaseCandidate,
    /// None means the provider did not supply an HTTP enclosure/link.
    pub download: Option<ReleaseDownload>,
}

pub struct AcquisitionPage {
    pub releases: Vec<AcquisitionRelease>,
    pub offset: u32,
    pub total: Option<u64>,
    pub next_offset: Option<u32>,
}

pub struct Prowlarr {
    http: Http,
    indexer_id: u32,
    protocol: ReleaseProtocol,
    categories: CategoryMap,
}

impl Prowlarr {
    pub fn new(
        config: ProviderConfig,
        indexer_id: u32,
        protocol: ReleaseProtocol,
        categories: CategoryMap,
    ) -> Result<Self, ProviderError> {
        // Prowlarr's zero indexer is a synthetic test endpoint, not a real source.
        if indexer_id == 0
            || indexer_id > i32::MAX as u32
            || [&categories.comics, &categories.manga, &categories.magazines]
                .iter()
                .any(|cats| cats.len() > 100 || cats.contains(&0))
        {
            return Err(ProviderError::InvalidConfiguration);
        }
        Ok(Self {
            http: Http::new(config, true)?,
            indexer_id,
            protocol,
            categories,
        })
    }

    fn request(&self, operation: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(self
            .http
            .key_query(
                self.http.get(&format!("{}/api", self.indexer_id))?,
                "apikey",
            )
            .header(reqwest::header::ACCEPT, "application/xml")
            .query(&[("t", operation), ("o", "xml")]))
    }

    pub async fn capabilities(&self) -> Result<IndexerCapabilities, ProviderError> {
        let body = self.http.body(self.request("caps")?).await?;
        let caps: Caps = xml(&body, b"caps")?;
        let mut categories = Vec::new();
        for cat in caps.categories.category {
            categories.push(cat.id);
            categories.extend(cat.subcat.into_iter().map(|c| c.id));
        }
        categories.sort_unstable();
        categories.dedup();
        if categories.contains(&0) || caps.limits.max == 0 {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(IndexerCapabilities {
            generic_search: caps.searching.search.available == "yes"
                && caps
                    .searching
                    .search
                    .supported_params
                    .is_none_or(|params| params.split(',').any(|p| p.trim() == "q")),
            categories,
            max_limit: caps.limits.max.min(100),
        })
    }

    pub async fn search(
        &self,
        query: &str,
        content_type: ContentType,
        page: SearchPage,
    ) -> Result<ReleasePage, ProviderError> {
        self.search_offset(query, content_type, page.offset()?, page.size)
            .await
    }

    /// Call again with next_offset, deduplicating by (configured source, indexer_id,
    /// guid) across pages in the caller. Search results are not an issue catalog.
    pub async fn search_offset(
        &self,
        query: &str,
        content_type: ContentType,
        offset: u32,
        limit: u32,
    ) -> Result<ReleasePage, ProviderError> {
        let page = self
            .search_impl(query, content_type, offset, limit, false)
            .await?;
        Ok(ReleasePage {
            releases: page.releases.into_iter().map(|r| r.release).collect(),
            offset: page.offset,
            total: page.total,
            next_offset: page.next_offset,
        })
    }

    /// Retains credentialed targets outside all public DTOs. No download occurs.
    pub async fn search_for_acquisition(
        &self,
        query: &str,
        content_type: ContentType,
        offset: u32,
        limit: u32,
    ) -> Result<AcquisitionPage, ProviderError> {
        self.search_impl(query, content_type, offset, limit, true)
            .await
    }

    /// Retrieves only a descriptor returned by acquisition search, using the same
    /// timeout/body bounds and redirect policy. This does not submit or import it.
    pub async fn retrieve_payload(
        &self,
        download: &ReleaseDownload,
    ) -> Result<Vec<u8>, ProviderError> {
        if download.indexer_id != self.indexer_id {
            return Err(ProviderError::Unsupported);
        }
        self.http.retrieve(&download.url).await
    }

    async fn search_impl(
        &self,
        query: &str,
        content_type: ContentType,
        offset: u32,
        limit: u32,
        retain_download: bool,
    ) -> Result<AcquisitionPage, ProviderError> {
        query_valid(query)?;
        if limit == 0 || limit > 100 || offset.checked_add(limit).is_none() {
            return Err(ProviderError::InvalidQuery);
        }
        let cats = match content_type {
            ContentType::Comic => &self.categories.comics,
            ContentType::Manga => &self.categories.manga,
            ContentType::Magazine => &self.categories.magazines,
        };
        if cats.is_empty() {
            return Err(ProviderError::Unsupported);
        }
        let caps = self.capabilities().await?;
        if !caps.generic_search || cats.iter().any(|c| !caps.categories.contains(c)) {
            return Err(ProviderError::Unsupported);
        }
        if limit > caps.max_limit {
            return Err(ProviderError::InvalidQuery);
        }
        let categories = cats
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let body = self
            .http
            .body(
                self.request("search")?
                    .query(&[("q", query), ("cat", &categories), ("extended", "1")])
                    .query(&[("offset", offset), ("limit", limit)]),
            )
            .await?;
        let rss: Rss = xml(&body, b"rss")?;
        let count = rss.channel.item.len();
        let total = rss.channel.response.as_ref().map(|r| r.total);
        if count > limit as usize
            || rss
                .channel
                .response
                .as_ref()
                .is_some_and(|r| r.offset != offset)
            || total.is_some_and(|total| count > 0 && u64::from(offset) + count as u64 > total)
        {
            return Err(ProviderError::InvalidResponse);
        }
        let mut seen = HashSet::new();
        let mut releases = Vec::new();
        for item in rss.channel.item {
            let mut categories = Vec::new();
            let mut size = None;
            for attr in item.attr {
                match attr.name.as_str() {
                    "category" => categories.push(
                        attr.value
                            .parse::<u32>()
                            .map_err(|_| ProviderError::InvalidResponse)?,
                    ),
                    "size" => {
                        let bytes = attr
                            .value
                            .parse::<u64>()
                            .map_err(|_| ProviderError::InvalidResponse)?;
                        if size.is_some_and(|old| old != bytes) {
                            return Err(ProviderError::InvalidResponse);
                        }
                        size = Some(bytes);
                    }
                    _ => {}
                }
            }
            categories.sort_unstable();
            categories.dedup();
            if categories.is_empty() || categories.contains(&0) {
                return Err(ProviderError::InvalidResponse);
            }
            let size_bytes = size
                .or(item.enclosure.as_ref().map(|e| e.length))
                .ok_or(ProviderError::InvalidResponse)?;
            if item.enclosure.as_ref().is_some_and(|e| {
                !matches!(
                    (self.protocol, e.kind.as_str()),
                    (ReleaseProtocol::Torrent, "application/x-bittorrent")
                        | (ReleaseProtocol::Usenet, "application/x-nzb")
                )
            }) {
                return Err(ProviderError::InvalidResponse);
            }
            let guid = self.http.text(&item.guid)?;
            let title = self.http.text(&item.title)?;
            if seen.insert(guid.clone()) {
                let download = if retain_download {
                    item.enclosure
                        .as_ref()
                        .and_then(|e| e.url.as_deref())
                        .or(item.link.as_deref())
                        .map(|url| {
                            self.http.download_target(url).map(|url| ReleaseDownload {
                                url,
                                indexer_id: self.indexer_id,
                            })
                        })
                        .transpose()?
                } else {
                    None
                };
                let release = ReleaseCandidate {
                    indexer_id: self.indexer_id,
                    guid,
                    title,
                    content_type: content_type.clone(),
                    categories,
                    size_bytes,
                    protocol: self.protocol,
                    published_at: item
                        .pub_date
                        .as_deref()
                        .map(|date| self.http.text(date))
                        .transpose()?,
                };
                releases.push(AcquisitionRelease { release, download });
            }
        }
        let next = offset + count as u32;
        let more =
            count > 0 && total.map_or(count == limit as usize, |total| u64::from(next) < total);
        Ok(AcquisitionPage {
            releases,
            offset,
            total,
            next_offset: more.then_some(next),
        })
    }
}

fn xml<T: serde::de::DeserializeOwned>(
    body: &[u8],
    expected_root: &[u8],
) -> Result<T, ProviderError> {
    let mut reader = Reader::from_reader(body);
    let mut depth = 0usize;
    let mut root = None;
    loop {
        match reader
            .read_event()
            .map_err(|_| ProviderError::InvalidResponse)?
        {
            Event::Start(tag) => {
                if depth == 0 {
                    if root.is_some() {
                        return Err(ProviderError::InvalidResponse);
                    }
                    root = Some(tag.name().as_ref().as_bytes().to_vec());
                }
                depth += 1;
                if depth > 32 {
                    return Err(ProviderError::InvalidResponse);
                }
            }
            Event::Empty(tag) if depth == 0 => {
                if root.is_some() {
                    return Err(ProviderError::InvalidResponse);
                }
                root = Some(tag.name().as_ref().as_bytes().to_vec());
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(ProviderError::InvalidResponse)?;
            }
            Event::DocType(_) => return Err(ProviderError::InvalidResponse),
            Event::GeneralRef(_) if depth == 0 => return Err(ProviderError::InvalidResponse),
            Event::Text(text)
                if depth == 0 && !text.as_ref().bytes().all(|b| b.is_ascii_whitespace()) =>
            {
                return Err(ProviderError::InvalidResponse);
            }
            Event::CData(_) if depth == 0 => return Err(ProviderError::InvalidResponse),
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ProviderError::InvalidResponse);
    }
    if root.as_deref() == Some(b"error") {
        let error: ApiError =
            quick_xml::de::from_reader(body).map_err(|_| ProviderError::InvalidResponse)?;
        return Err(if matches!(error.code, 429 | 500 | 501) {
            ProviderError::RateLimited {
                retry_after_seconds: None,
            }
        } else {
            ProviderError::Unavailable
        });
    }
    if root.as_deref() != Some(expected_root) {
        return Err(ProviderError::InvalidResponse);
    }
    quick_xml::de::from_reader(body).map_err(|_| ProviderError::InvalidResponse)
}

#[derive(Deserialize)]
struct Caps {
    limits: Limits,
    searching: Searching,
    categories: Categories,
}
#[derive(Deserialize)]
struct Limits {
    #[serde(rename = "@max")]
    max: u32,
}
#[derive(Deserialize)]
struct Searching {
    search: Search,
}
#[derive(Deserialize)]
struct Search {
    #[serde(rename = "@available")]
    available: String,
    #[serde(rename = "@supportedParams")]
    supported_params: Option<String>,
}
#[derive(Deserialize)]
struct Categories {
    #[serde(default)]
    category: Vec<Category>,
}
#[derive(Deserialize)]
struct Category {
    #[serde(rename = "@id")]
    id: u32,
    #[serde(default)]
    subcat: Vec<Subcategory>,
}
#[derive(Deserialize)]
struct Subcategory {
    #[serde(rename = "@id")]
    id: u32,
}
#[derive(Deserialize)]
struct Rss {
    channel: Channel,
}
#[derive(Deserialize)]
struct Channel {
    #[serde(default)]
    item: Vec<Item>,
    response: Option<RssResponse>,
}
#[derive(Deserialize)]
struct RssResponse {
    #[serde(rename = "@offset")]
    offset: u32,
    #[serde(rename = "@total")]
    total: u64,
}
#[derive(Deserialize)]
struct Item {
    title: String,
    guid: String,
    #[serde(rename = "pubDate")]
    pub_date: Option<String>,
    enclosure: Option<Enclosure>,
    link: Option<String>,
    #[serde(default)]
    attr: Vec<Attr>,
}
#[derive(Deserialize)]
struct Enclosure {
    #[serde(rename = "@url")]
    url: Option<String>,
    #[serde(rename = "@length")]
    length: u64,
    #[serde(rename = "@type")]
    kind: String,
}
#[derive(Deserialize)]
struct Attr {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@value")]
    value: String,
}
#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "@code")]
    code: u32,
}
