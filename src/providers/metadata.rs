use super::{
    ContentType, MetadataCandidate, MetadataPage, MetadataProvider, ProviderConfig, ProviderError,
    SearchPage, http::Http, query_valid, year,
};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

pub struct ComicVine {
    http: Http,
}
pub struct MangaUpdates {
    http: Http,
}
pub struct MangaDex {
    http: Http,
}

impl ComicVine {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, true)?,
        })
    }

    /// Searches volumes (publication runs), not individual issues.
    pub async fn search(
        &self,
        query: &str,
        page: SearchPage,
    ) -> Result<MetadataPage, ProviderError> {
        query_valid(query)?;
        let offset = page.offset()?;
        let request = self
            .http
            .key_query(self.http.get("search/")?, "api_key")
            .header(ACCEPT, "application/json")
            .query(&[
                ("query", query),
                ("format", "json"),
                ("resources", "volume"),
                ("field_list", "id,name,start_year,resource_type"),
            ])
            .query(&[("limit", page.size), ("offset", offset)]);
        let body = self.http.body(request).await?;
        // Status is checked before results: failure envelopes may omit results.
        #[derive(Deserialize)]
        struct Status {
            status_code: u32,
        }
        match decode::<Status>(&body)?.status_code {
            1 => {}
            107 => {
                return Err(ProviderError::RateLimited {
                    retry_after_seconds: None,
                });
            }
            _ => return Err(ProviderError::Unavailable),
        }
        let result: CvSearch = decode(&body)?;
        if result.offset != offset {
            return Err(ProviderError::InvalidResponse);
        }
        let count = result.results.len();
        let candidates = result
            .results
            .into_iter()
            .map(|item| {
                if item.id == 0 || item.resource_type != "volume" {
                    return Err(ProviderError::InvalidResponse);
                }
                Ok(MetadataCandidate {
                    provider: MetadataProvider::ComicVine,
                    external_id: format!("4050-{}", item.id),
                    title: self.http.text(&item.name)?,
                    content_type: ContentType::Comic,
                    date: year(item.start_year.as_deref()),
                })
            })
            .collect::<Result<_, _>>()?;
        finish(candidates, count, result.number_of_total_results, page)
    }
}

impl MangaUpdates {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, false)?,
        })
    }

    /// Public series search, without account authentication.
    pub async fn search(
        &self,
        query: &str,
        page: SearchPage,
    ) -> Result<MetadataPage, ProviderError> {
        query_valid(query)?;
        page.offset()?;
        let request = self
            .http
            .post("series/search")?
            .header(ACCEPT, "application/json")
            .json(
                &serde_json::json!({ "search": query, "stype": "title", "page": page.number,
                "perpage": page.size, "type": ["Manga", "Manhwa", "Manhua", "OEL", "Doujinshi"] }),
            );
        let result: MuSearch = decode(&self.http.body(request).await?)?;
        if result.page != page.number || result.per_page == 0 || result.per_page > 100 {
            return Err(ProviderError::InvalidResponse);
        }
        let count = result.results.len();
        let candidates = result
            .results
            .into_iter()
            .filter(|item| {
                matches!(
                    item.record.kind.as_str(),
                    "Manga" | "Manhwa" | "Manhua" | "OEL" | "Doujinshi"
                )
            })
            .map(|item| {
                let item = item.record;
                if item.series_id == 0 {
                    return Err(ProviderError::InvalidResponse);
                }
                Ok(MetadataCandidate {
                    provider: MetadataProvider::MangaUpdates,
                    external_id: item.series_id.to_string(),
                    title: self.http.text(&item.title)?,
                    content_type: ContentType::Manga,
                    date: year(item.year.as_deref()),
                })
            })
            .collect::<Result<_, _>>()?;
        // The live service can normalize perpage (e.g. 1 to 25). Carry the
        // effective size forward instead of rejecting a valid response or skipping.
        finish(
            candidates,
            count,
            result.total_hits,
            SearchPage {
                number: page.number,
                size: result.per_page,
            },
        )
    }
}

impl MangaDex {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, false)?,
        })
    }

    /// Public manga metadata only; this does not fetch chapter pages.
    pub async fn search(
        &self,
        query: &str,
        page: SearchPage,
    ) -> Result<MetadataPage, ProviderError> {
        query_valid(query)?;
        let offset = page.offset()?;
        if offset.checked_add(page.size).is_none_or(|end| end > 10000) {
            return Err(ProviderError::InvalidQuery);
        }
        let request = self
            .http
            .get("manga")?
            .header(ACCEPT, "application/json")
            .query(&[("title", query), ("order[relevance]", "desc")])
            .query(&[("limit", page.size), ("offset", offset)]);
        let result: MdSearch = decode(&self.http.body(request).await?)?;
        if result.result != "ok" || result.offset != offset || result.limit != page.size {
            return Err(ProviderError::InvalidResponse);
        }
        let count = result.data.len();
        let candidates = result
            .data
            .into_iter()
            .map(|item| {
                if item.kind != "manga" || uuid::Uuid::parse_str(&item.id).is_err() {
                    return Err(ProviderError::InvalidResponse);
                }
                let attrs = item.attributes;
                let title = attrs
                    .title
                    .get("en")
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| {
                        attrs
                            .original_language
                            .as_ref()
                            .and_then(|lang| attrs.title.get(lang))
                            .filter(|s| !s.trim().is_empty())
                    })
                    .or_else(|| attrs.title.values().find(|s| !s.trim().is_empty()))
                    .ok_or(ProviderError::InvalidResponse)?;
                Ok(MetadataCandidate {
                    provider: MetadataProvider::MangaDex,
                    external_id: self.http.text(&item.id)?,
                    title: self.http.text(title)?,
                    content_type: ContentType::Manga,
                    date: year(attrs.year.map(|v| v.to_string()).as_deref()),
                })
            })
            .collect::<Result<_, _>>()?;
        let mut result = finish(candidates, count, result.total, page)?;
        if offset + page.size >= 10000 {
            result.next_page = None;
        }
        Ok(result)
    }
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(body).map_err(|_| ProviderError::InvalidResponse)
}

fn finish(
    mut candidates: Vec<MetadataCandidate>,
    count: usize,
    total: u64,
    page: SearchPage,
) -> Result<MetadataPage, ProviderError> {
    let offset = u64::from(page.offset()?);
    if count > page.size as usize || (count > 0 && offset + count as u64 > total) {
        return Err(ProviderError::InvalidResponse);
    }
    let mut seen = HashSet::new();
    candidates.retain(|c| seen.insert(c.external_id.clone()));
    Ok(MetadataPage {
        candidates,
        total,
        page_size: page.size,
        next_page: if count > 0 && offset + (count as u64) < total {
            page.number.checked_add(1)
        } else {
            None
        },
    })
}

#[derive(Deserialize)]
struct CvSearch {
    results: Vec<CvVolume>,
    number_of_total_results: u64,
    offset: u32,
}
#[derive(Deserialize)]
struct CvVolume {
    id: u64,
    name: String,
    start_year: Option<String>,
    resource_type: String,
}
#[derive(Deserialize)]
struct MuSearch {
    results: Vec<MuResult>,
    total_hits: u64,
    page: u32,
    per_page: u32,
}
#[derive(Deserialize)]
struct MuResult {
    record: MuSeries,
}
#[derive(Deserialize)]
struct MuSeries {
    series_id: u64,
    title: String,
    year: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct MdSearch {
    result: String,
    data: Vec<MdManga>,
    total: u64,
    limit: u32,
    offset: u32,
}
#[derive(Deserialize)]
struct MdManga {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    attributes: MdAttributes,
}
#[derive(Deserialize)]
struct MdAttributes {
    title: BTreeMap<String, String>,
    year: Option<u32>,
    #[serde(rename = "originalLanguage")]
    original_language: Option<String>,
}
