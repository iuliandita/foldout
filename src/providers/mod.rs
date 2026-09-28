//! Bounded HTTP adapters. Construct configurations from administrator-owned settings,
//! never request parameters. See README.md for coverage and provider policies.
mod capabilities;
pub mod getcomics;
mod http;
mod indexer;
pub mod internet_archive;
pub mod mangadex_chapters;
mod metadata;
pub mod release_evidence;

pub use crate::catalog::ContentType;
pub use capabilities::*;
pub use http::{HttpLimits, ProviderConfig};
pub use indexer::*;
pub use metadata::{ComicVine, MangaDex, MangaUpdates};
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    #[error("provider unavailable")]
    Unavailable,
    #[error("provider rate limited")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("invalid provider response")]
    InvalidResponse,
    #[error("invalid provider configuration")]
    InvalidConfiguration,
    #[error("invalid provider query")]
    InvalidQuery,
    #[error("provider capability unavailable")]
    Unsupported,
    #[error("provider challenge requires user action")]
    ChallengeRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataProvider {
    ComicVine,
    MangaUpdates,
    MangaDex,
    LocalManual,
    Issn,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetadataCandidate {
    pub provider: MetadataProvider,
    pub external_id: String,
    pub title: String,
    pub content_type: ContentType,
    /// Publication year only, when known. Never an API record creation timestamp.
    pub date: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetadataPage {
    pub candidates: Vec<MetadataCandidate>,
    pub total: u64,
    /// Effective server page size. Use this size when requesting next_page.
    pub page_size: u32,
    /// One-based page, using the same page size as the preceding call.
    pub next_page: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchPage {
    pub number: u32,
    pub size: u32,
}

impl Default for SearchPage {
    fn default() -> Self {
        Self {
            number: 1,
            size: 20,
        }
    }
}

impl SearchPage {
    pub(crate) fn offset(self) -> Result<u32, ProviderError> {
        if self.number == 0 || self.size == 0 || self.size > 100 {
            return Err(ProviderError::InvalidQuery);
        }
        (self.number - 1)
            .checked_mul(self.size)
            .ok_or(ProviderError::InvalidQuery)
    }
}

pub(crate) fn query_valid(query: &str) -> Result<(), ProviderError> {
    if query.trim().is_empty() || query.len() > 512 || query.chars().any(char::is_control) {
        return Err(ProviderError::InvalidQuery);
    }
    Ok(())
}

pub(crate) fn year(value: Option<&str>) -> Option<String> {
    value
        .filter(|v| v.len() == 4 && v.bytes().all(|b| b.is_ascii_digit()) && *v != "0000")
        .map(str::to_owned)
}
