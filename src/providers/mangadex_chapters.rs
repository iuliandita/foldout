//! Chapter metadata and short-lived transfer manifests; no image requests.
use super::{ProviderConfig, ProviderError, SearchPage, http::Http};
use reqwest::{Url, header::ACCEPT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_OFFSET_END: u32 = 10_000;
const MAX_PAGES: u32 = 1_000;

pub struct MangaDexChapters {
    http: Http,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MangaDexScanlationGroup {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MangaDexChapter {
    pub id: String,
    pub manga_id: String,
    pub language: String,
    /// Labels remain text, including fractional chapters and null/empty labels.
    pub chapter: Option<String>,
    pub volume: Option<String>,
    pub title: Option<String>,
    pub scanlation_groups: Vec<MangaDexScanlationGroup>,
    /// External chapters are listed but cannot produce an at-home manifest.
    pub external_url: Option<String>,
    pub page_count: u32,
    pub version: u32,
    pub is_unavailable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MangaDexChapterPage {
    pub chapters: Vec<MangaDexChapter>,
    pub total: u64,
    pub page_size: u32,
    pub next_page: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MangaDexManifestSummary {
    pub chapter_id: String,
    pub hash: String,
    pub page_count: u32,
}

/// Opaque and intentionally not Serialize. Debug reveals only the public summary.
/// Retain in memory only: the server URL expires and may contain an access token.
pub struct MangaDexManifest {
    summary: MangaDexManifestSummary,
    base: Url,
    filenames: Vec<String>,
    identity: String,
}

impl std::fmt::Debug for MangaDexManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.summary, f)
    }
}

impl MangaDexManifest {
    pub fn summary(&self) -> &MangaDexManifestSummary {
        &self.summary
    }

    /// SHA-256 of length-prefixed hash and ordered original-quality filenames.
    /// Independent of the temporary server URL; changes when content/order changes.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Zero-based original-quality page. Future workers must also validate resolved
    /// IPs and redirects before fetching; hostname syntax alone cannot stop rebinding.
    pub(crate) fn page_url(&self, index: usize) -> Result<Url, ProviderError> {
        let filename = self
            .filenames
            .get(index)
            .ok_or(ProviderError::InvalidQuery)?;
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| ProviderError::InvalidResponse)?
            .pop_if_empty()
            .push("data")
            .push(&self.summary.hash)
            .push(filename);
        Ok(url)
    }
}

impl MangaDexChapters {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, false)?,
        })
    }

    /// One bounded page for an explicit manga UUID and translated language.
    /// Callers aggregating pages must reject repeated chapter IDs across pages too.
    pub async fn list_chapters(
        &self,
        manga_id: &str,
        language: &str,
        page: SearchPage,
    ) -> Result<MangaDexChapterPage, ProviderError> {
        let manga_id = canonical_uuid(manga_id).ok_or(ProviderError::InvalidQuery)?;
        if !valid_language(language) {
            return Err(ProviderError::InvalidQuery);
        }
        let offset = page.offset()?;
        if offset
            .checked_add(page.size)
            .is_none_or(|end| end > MAX_OFFSET_END)
        {
            return Err(ProviderError::InvalidQuery);
        }
        let request = self
            .http
            .get("chapter")?
            .header(ACCEPT, "application/json")
            .query(&[
                ("manga", manga_id.as_str()),
                ("translatedLanguage[]", language),
                ("includes[]", "scanlation_group"),
                ("order[chapter]", "asc"),
                ("order[createdAt]", "asc"),
                // Omit external/empty filters: 1 requires that property, rather than including it.
                ("includeFutureUpdates", "0"),
                ("includeFuturePublishAt", "0"),
                ("includeUnavailable", "1"),
            ])
            .query(&[("limit", page.size), ("offset", offset)]);
        let response: ChapterList = decode(&self.http.body(request).await?)?;
        let count = response.data.len() as u64;
        if response.result != "ok"
            || response.response != "collection"
            || response.limit != page.size
            || response.offset != offset
            || count > u64::from(page.size)
            || count
                != response
                    .total
                    .saturating_sub(u64::from(offset))
                    .min(u64::from(page.size))
        {
            return Err(ProviderError::InvalidResponse);
        }
        let mut seen = HashSet::new();
        let mut chapters = Vec::with_capacity(response.data.len());
        for item in response.data {
            let chapter = self.parse_chapter(item)?;
            if !seen.insert(chapter.id.clone())
                || chapter.manga_id != manga_id
                || chapter.language != language
            {
                return Err(ProviderError::InvalidResponse);
            }
            chapters.push(chapter);
        }
        let end = offset + page.size;
        Ok(MangaDexChapterPage {
            chapters,
            total: response.total,
            page_size: page.size,
            next_page: (count > 0 && u64::from(end) < response.total && end < MAX_OFFSET_END)
                .then(|| page.number + 1),
        })
    }

    /// Fetch one exact chapter's metadata; callers must compare it with their selection.
    pub async fn get_chapter(&self, id: &str) -> Result<MangaDexChapter, ProviderError> {
        let id = canonical_uuid(id).ok_or(ProviderError::InvalidQuery)?;
        let request = self
            .http
            .get(&format!("chapter/{id}"))?
            .header(ACCEPT, "application/json")
            .query(&[("includes[]", "scanlation_group")]);
        let response: ChapterEntity = decode(&self.http.body(request).await?)?;
        if response.result != "ok" || response.response != "entity" {
            return Err(ProviderError::InvalidResponse);
        }
        let chapter = self.parse_chapter(response.data)?;
        if chapter.id != id {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(chapter)
    }

    fn parse_chapter(&self, item: Chapter) -> Result<MangaDexChapter, ProviderError> {
        let id = canonical_uuid(&item.id).ok_or(ProviderError::InvalidResponse)?;
        let attrs = item.attributes;
        if item.kind != "chapter"
            || !valid_language(&attrs.translated_language)
            || attrs.pages > MAX_PAGES
            || attrs.version == 0
            || item.relationships.len() > 128
        {
            return Err(ProviderError::InvalidResponse);
        }
        let mut manga_id = None;
        let mut group_ids = HashSet::new();
        let mut groups = Vec::new();
        for relation in item.relationships {
            match relation.kind.as_str() {
                "manga" => {
                    if manga_id.is_some() {
                        return Err(ProviderError::InvalidResponse);
                    }
                    manga_id =
                        Some(canonical_uuid(&relation.id).ok_or(ProviderError::InvalidResponse)?);
                }
                "scanlation_group" => {
                    let id = canonical_uuid(&relation.id).ok_or(ProviderError::InvalidResponse)?;
                    if !group_ids.insert(id.clone()) {
                        return Err(ProviderError::InvalidResponse);
                    }
                    let name = relation
                        .attributes
                        .and_then(|a| a.name)
                        .ok_or(ProviderError::InvalidResponse)?;
                    groups.push(MangaDexScanlationGroup {
                        id,
                        name: self.http.text(&name)?,
                    });
                }
                _ => {}
            }
        }
        let manga_id = manga_id.ok_or(ProviderError::InvalidResponse)?;
        let external_url = attrs
            .external_url
            .map(|value| {
                if value.len() > 512 {
                    return Err(ProviderError::InvalidResponse);
                }
                let url = Url::parse(&value).map_err(|_| ProviderError::InvalidResponse)?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(ProviderError::InvalidResponse);
                }
                self.http.text(&value)?;
                Ok(value)
            })
            .transpose()?;
        Ok(MangaDexChapter {
            id,
            manga_id,
            language: attrs.translated_language,
            chapter: self.label(attrs.chapter, 8)?,
            volume: self.label(attrs.volume, 64)?,
            title: self.label(attrs.title, 255)?,
            scanlation_groups: groups,
            external_url,
            page_count: attrs.pages,
            version: attrs.version,
            is_unavailable: attrs.is_unavailable,
        })
    }

    /// Metadata must come from list_chapters or get_chapter. Requests a manifest, never images.
    pub async fn at_home(
        &self,
        chapter: &MangaDexChapter,
    ) -> Result<MangaDexManifest, ProviderError> {
        let id = canonical_uuid(&chapter.id).ok_or(ProviderError::InvalidQuery)?;
        if canonical_uuid(&chapter.manga_id).is_none()
            || !valid_language(&chapter.language)
            || chapter.page_count > MAX_PAGES
            || chapter.version == 0
        {
            return Err(ProviderError::InvalidQuery);
        }
        if chapter.external_url.is_some() || chapter.is_unavailable || chapter.page_count == 0 {
            return Err(ProviderError::Unsupported);
        }
        let request = self
            .http
            .get(&format!("at-home/server/{id}"))?
            .header(ACCEPT, "application/json")
            .query(&[("forcePort443", true)]);
        let response: AtHome = decode(&self.http.body(request).await?)?;
        if response.result != "ok"
            || response.chapter.hash.len() != 32
            || !response.chapter.hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ProviderError::InvalidResponse);
        }
        let base = manifest_base(&response.base_url)?;
        validate_files(&response.chapter.data, chapter.page_count)?;
        validate_files(&response.chapter.data_saver, chapter.page_count)?;
        let mut digest = Sha256::new();
        for value in std::iter::once(&response.chapter.hash).chain(response.chapter.data.iter()) {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        Ok(MangaDexManifest {
            summary: MangaDexManifestSummary {
                chapter_id: id,
                hash: response.chapter.hash,
                page_count: chapter.page_count,
            },
            base,
            filenames: response.chapter.data,
            identity: digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }

    fn label(&self, value: Option<String>, max: usize) -> Result<Option<String>, ProviderError> {
        if let Some(value) = &value {
            if value.chars().count() > max || value.chars().any(char::is_control) {
                return Err(ProviderError::InvalidResponse);
            }
            if !value.trim().is_empty() {
                self.http.text(value)?;
            }
        }
        Ok(value)
    }
}

fn canonical_uuid(value: &str) -> Option<String> {
    if value.len() != 36 {
        return None;
    }
    let id = uuid::Uuid::parse_str(value).ok()?;
    (!id.is_nil() && id.hyphenated().to_string().eq_ignore_ascii_case(value))
        .then(|| id.hyphenated().to_string())
}

fn valid_language(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes.len(), 2 | 5)
        && bytes[..2].iter().all(u8::is_ascii_lowercase)
        && (bytes.len() == 2 || (bytes[2] == b'-' && bytes[3..].iter().all(u8::is_ascii_lowercase)))
}

fn manifest_base(value: &str) -> Result<Url, ProviderError> {
    if value.len() > 8192
        || !value.starts_with("https://")
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'\\')
    {
        return Err(ProviderError::InvalidResponse);
    }
    let url = Url::parse(value).map_err(|_| ProviderError::InvalidResponse)?;
    let host = url.host_str().ok_or(ProviderError::InvalidResponse)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || !public_hostname(host)
        || value[8..]
            .split('/')
            .next()
            .is_some_and(|authority| authority.contains('@'))
    {
        return Err(ProviderError::InvalidResponse);
    }
    Ok(url)
}

fn public_hostname(host: &str) -> bool {
    if host.len() > 253
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.starts_with('[')
        || !host.contains('.')
        || host.ends_with('.')
    {
        return false;
    }
    let suffix = host.rsplit('.').next().unwrap_or("");
    if [
        "localhost",
        "local",
        "internal",
        "lan",
        "home",
        "test",
        "invalid",
        "example",
        "onion",
    ]
    .contains(&suffix)
        || suffix.bytes().all(|b| b.is_ascii_digit())
        || host == "home.arpa"
        || host.ends_with(".home.arpa")
    {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

fn validate_files(files: &[String], expected: u32) -> Result<(), ProviderError> {
    let mut seen = HashSet::new();
    if files.len() != expected as usize {
        return Err(ProviderError::InvalidResponse);
    }
    for name in files {
        let Some((stem, extension)) = name.rsplit_once('.') else {
            return Err(ProviderError::InvalidResponse);
        };
        if name.len() > 255
            || stem.is_empty()
            || name.starts_with('.')
            || name.contains("..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || !["jpg", "jpeg", "png", "webp"].contains(&extension.to_ascii_lowercase().as_str())
            || !seen.insert(name.to_ascii_lowercase())
        {
            return Err(ProviderError::InvalidResponse);
        }
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(body).map_err(|_| ProviderError::InvalidResponse)
}

#[derive(Deserialize)]
struct ChapterList {
    result: String,
    response: String,
    data: Vec<Chapter>,
    limit: u32,
    offset: u32,
    total: u64,
}
#[derive(Deserialize)]
struct ChapterEntity {
    result: String,
    response: String,
    data: Chapter,
}
#[derive(Deserialize)]
struct Chapter {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    attributes: ChapterAttributes,
    relationships: Vec<Relationship>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChapterAttributes {
    title: Option<String>,
    volume: Option<String>,
    chapter: Option<String>,
    pages: u32,
    translated_language: String,
    external_url: Option<String>,
    version: u32,
    #[serde(default)]
    is_unavailable: bool,
}
#[derive(Deserialize)]
struct Relationship {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    attributes: Option<GroupAttributes>,
}
#[derive(Deserialize)]
struct GroupAttributes {
    name: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AtHome {
    result: String,
    base_url: String,
    chapter: AtHomeChapter,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AtHomeChapter {
    hash: String,
    data: Vec<String>,
    data_saver: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_url_preserves_temporary_base_path_and_bounds_index() {
        for base in [
            "https://node.mangadex.network/temporary-secret",
            "https://node.mangadex.network/temporary-secret/",
        ] {
            let manifest = MangaDexManifest {
                summary: MangaDexManifestSummary {
                    chapter_id: "22222222-2222-4222-8222-222222222222".into(),
                    hash: "0123456789abcdef0123456789abcdef".into(),
                    page_count: 1,
                },
                base: manifest_base(base).unwrap(),
                filenames: vec!["1-a.png".into()],
                identity: String::new(),
            };
            assert_eq!(
                manifest.page_url(0).unwrap().as_str(),
                "https://node.mangadex.network/temporary-secret/data/0123456789abcdef0123456789abcdef/1-a.png"
            );
            assert_eq!(manifest.page_url(1), Err(ProviderError::InvalidQuery));
        }
    }
}
