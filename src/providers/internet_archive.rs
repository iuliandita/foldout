//! Anonymous magazine discovery only. Availability is metadata eligibility, not access verification.
use super::{ProviderConfig, ProviderError, SearchPage, http::Http, query_valid};
use reqwest::{Url, header::ACCEPT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

const MAX_RESULTS: u32 = 10_000;
const MAX_FILES: usize = 2_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchiveMagazine {
    pub identifier: String,
    pub title: String,
    /// Source values, without inferred precision, language codes, or regional editions.
    pub dates: Vec<String>,
    pub languages: Vec<String>,
    pub countries: Vec<String>,
    pub coverage: Vec<String>,
    pub volumes: Vec<String>,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchivePage {
    pub items: Vec<ArchiveMagazine>,
    pub total: u64,
    pub page_size: u32,
    pub next_page: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEligibility {
    /// Supported file with observed identity and no reported restriction; access is unverified.
    Available,
    Restricted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EligibilityReason {
    MetadataEligible,
    AccessRestricted,
    ItemUnavailable,
    UnsupportedFormat,
    IncompleteIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchiveFile {
    pub identifier: String,
    /// Exact source spelling, never trimmed, decoded, or used as a local path.
    pub name: String,
    pub format: Option<String>,
    pub size: Option<u64>,
    pub sha1: Option<String>,
    pub md5: Option<String>,
    pub eligibility: FileEligibility,
    pub reason: EligibilityReason,
    /// Canonical reference only. This adapter never requests it.
    pub download_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchiveItem {
    pub magazine: ArchiveMagazine,
    pub files: Vec<ArchiveFile>,
}

pub struct InternetArchive {
    http: Http,
}

impl InternetArchive {
    /// Configuration is administrator-owned. Production uses https://archive.org/ without a key.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, false)?,
        })
    }

    pub async fn search(
        &self,
        query: &str,
        page: SearchPage,
    ) -> Result<ArchivePage, ProviderError> {
        query_valid(query)?;
        let offset = page.offset()?;
        if offset
            .checked_add(page.size)
            .is_none_or(|end| end > MAX_RESULTS)
        {
            return Err(ProviderError::InvalidQuery);
        }
        let escaped = query.trim().replace('\\', "\\\\").replace('"', "\\\"");
        let query =
            format!("collection:magazine_rack AND mediatype:texts AND (title:\"{escaped}\")");
        let request = self.http.get("advancedsearch.php")?.header(ACCEPT, "application/json")
            .query(&[("q", query), ("rows", page.size.to_string()), ("page", page.number.to_string()),
                ("output", "json".into()), ("sort[]", "identifier asc".into()),
                ("fl[]", "identifier,title,date,language,country,coverage,volume,issue,collection,mediatype".into())]);
        let response: SearchResponse = serde_json::from_slice(&self.http.body(request).await?)
            .map_err(|_| ProviderError::InvalidResponse)?;
        let response = response.response;
        let expected =
            u64::from(page.size).min(response.num_found.saturating_sub(u64::from(offset)));
        if response.start != offset || response.docs.len() as u64 != expected {
            return Err(ProviderError::InvalidResponse);
        }
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for doc in response.docs {
            let item = self.magazine(&doc)?;
            if !seen.insert(item.identifier.clone()) {
                return Err(ProviderError::InvalidResponse);
            }
            items.push(item);
        }
        let next_page = (items.len() == page.size as usize
            && u64::from(offset + page.size) < response.num_found
            && offset + page.size * 2 <= MAX_RESULTS)
            .then_some(page.number + 1);
        Ok(ArchivePage {
            items,
            total: response.num_found,
            page_size: page.size,
            next_page,
        })
    }

    pub async fn item(&self, identifier: &str) -> Result<ArchiveItem, ProviderError> {
        if !valid_identifier(identifier) {
            return Err(ProviderError::InvalidQuery);
        }
        let request = self
            .http
            .get(&format!("metadata/{identifier}"))?
            .header(ACCEPT, "application/json");
        let root: Value = serde_json::from_slice(&self.http.body(request).await?)
            .map_err(|_| ProviderError::InvalidResponse)?;
        if root.get("error").is_some() || root.as_array().is_some_and(Vec::is_empty) {
            return Err(ProviderError::Unavailable);
        }
        let metadata = &root["metadata"];
        let magazine = self.magazine(metadata)?;
        if magazine.identifier != identifier {
            return Err(ProviderError::InvalidResponse);
        }
        let unavailable = flag(&root, "servers_unavailable")?
            | flag(&root, "is_dark")?
            | flag(&root, "nodownload")?
            | flag(&root, "is_collection")?;
        let restricted = flag(metadata, "access-restricted-item")?
            | flag(metadata, "nodownload")?
            | flag(&root, "access-restricted-item")?;
        let records = root["files"]
            .as_array()
            .ok_or(ProviderError::InvalidResponse)?;
        if records.len() > MAX_FILES {
            return Err(ProviderError::InvalidResponse);
        }
        if let Some(count) = root.get("files_count")
            && count.as_u64() != Some(records.len() as u64)
        {
            return Err(ProviderError::InvalidResponse);
        }
        let mut names = HashSet::new();
        let mut files = Vec::new();
        for record in records {
            let name = record["name"]
                .as_str()
                .ok_or(ProviderError::InvalidResponse)?;
            if !valid_filename(name) || !names.insert(name) {
                return Err(ProviderError::InvalidResponse);
            }
            self.http.text(name)?;
            let format = self.optional_text(record, "format")?;
            let size = match record.get("size") {
                None | Some(Value::Null) => None,
                Some(Value::String(s))
                    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    Some(
                        s.parse::<u64>()
                            .map_err(|_| ProviderError::InvalidResponse)?,
                    )
                }
                Some(v) => Some(v.as_u64().ok_or(ProviderError::InvalidResponse)?),
            };
            let sha1 = checksum(record, "sha1", 40)?;
            let md5 = checksum(record, "md5", 32)?;
            let private = flag(record, "private")?;
            let extension = name
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase());
            let supported = matches!(
                (extension.as_deref(), format.as_deref()),
                (
                    Some("pdf"),
                    Some("Text PDF" | "Image Container PDF" | "PDF")
                ) | (Some("epub"), Some("EPUB"))
                    | (Some("cbz"), Some("Comic Book ZIP"))
            );
            let (eligibility, reason) = if restricted || private {
                (
                    FileEligibility::Restricted,
                    EligibilityReason::AccessRestricted,
                )
            } else if unavailable {
                (
                    FileEligibility::Unavailable,
                    EligibilityReason::ItemUnavailable,
                )
            } else if !supported {
                (
                    FileEligibility::Unavailable,
                    EligibilityReason::UnsupportedFormat,
                )
            } else if size.is_none_or(|s| s == 0) || (sha1.is_none() && md5.is_none()) {
                (
                    FileEligibility::Unavailable,
                    EligibilityReason::IncompleteIdentity,
                )
            } else {
                (
                    FileEligibility::Available,
                    EligibilityReason::MetadataEligible,
                )
            };
            let download_url = if eligibility == FileEligibility::Available {
                let mut url = Url::parse("https://archive.org/download/")
                    .map_err(|_| ProviderError::InvalidConfiguration)?;
                url.path_segments_mut()
                    .map_err(|_| ProviderError::InvalidConfiguration)?
                    .pop_if_empty()
                    .push(identifier)
                    .extend(name.split('/'));
                Some(url.to_string())
            } else {
                None
            };
            files.push(ArchiveFile {
                identifier: identifier.into(),
                name: name.into(),
                format,
                size,
                sha1,
                md5,
                eligibility,
                reason,
                download_url,
            });
        }
        Ok(ArchiveItem { magazine, files })
    }

    fn magazine(&self, value: &Value) -> Result<ArchiveMagazine, ProviderError> {
        let identifier = value["identifier"]
            .as_str()
            .ok_or(ProviderError::InvalidResponse)?;
        if !valid_identifier(identifier)
            || value["mediatype"] != "texts"
            || !self
                .texts(value, "collection")?
                .iter()
                .any(|c| c == "magazine_rack")
        {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(ArchiveMagazine {
            identifier: identifier.into(),
            title: self
                .optional_text(value, "title")?
                .ok_or(ProviderError::InvalidResponse)?,
            dates: self.texts(value, "date")?,
            languages: self.texts(value, "language")?,
            countries: self.texts(value, "country")?,
            coverage: self.texts(value, "coverage")?,
            volumes: self.texts(value, "volume")?,
            issues: self.texts(value, "issue")?,
        })
    }

    fn optional_text(&self, value: &Value, field: &str) -> Result<Option<String>, ProviderError> {
        match value.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => self.http.text(s).map(Some),
            _ => Err(ProviderError::InvalidResponse),
        }
    }

    fn texts(&self, value: &Value, field: &str) -> Result<Vec<String>, ProviderError> {
        match value.get(field) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(vec![self.http.text(s)?]),
            Some(Value::Array(values)) if values.len() <= 64 => values
                .iter()
                .map(|v| {
                    self.http
                        .text(v.as_str().ok_or(ProviderError::InvalidResponse)?)
                })
                .collect(),
            _ => Err(ProviderError::InvalidResponse),
        }
    }
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn valid_filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '%' | '?' | '#'))
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn flag(value: &Value, field: &str) -> Result<bool, ProviderError> {
    match value.get(field) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s == "true" || s == "1" => Ok(true),
        Some(Value::String(s)) if s == "false" || s == "0" => Ok(false),
        Some(Value::Number(n)) if n.as_u64() == Some(1) => Ok(true),
        Some(Value::Number(n)) if n.as_u64() == Some(0) => Ok(false),
        _ => Err(ProviderError::InvalidResponse),
    }
}

fn checksum(value: &Value, field: &str, length: usize) -> Result<Option<String>, ProviderError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() == length && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Ok(Some(s.clone()))
        }
        _ => Err(ProviderError::InvalidResponse),
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    response: SearchResults,
}
#[derive(Deserialize)]
struct SearchResults {
    #[serde(rename = "numFound")]
    num_found: u64,
    start: u32,
    docs: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_components_reject_traversal_and_url_syntax() {
        for id in ["", "../a", "a/b", "a?b", "%61", "-a", "a\\b"] {
            assert!(!valid_identifier(id));
        }
        assert!(valid_identifier("Ensign_Magazine-2016-02"));
        for name in [
            "../a.pdf",
            "a/../b.pdf",
            "/a.pdf",
            "a//b.pdf",
            "a%2fb.pdf",
            "a?b.pdf",
        ] {
            assert!(!valid_filename(name));
        }
        assert!(valid_filename("Issue scans/Issue 01.pdf"));
    }
}
