use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Comic,
    Manga,
    Magazine,
}

impl ContentType {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Comic => "comic",
            Self::Manga => "manga",
            Self::Magazine => "magazine",
        }
    }
    pub(crate) fn parse(value: String) -> Result<Self, String> {
        match value.as_str() {
            "comic" => Ok(Self::Comic),
            "manga" => Ok(Self::Manga),
            "magazine" => Ok(Self::Magazine),
            _ => Err(format!("invalid content type: {value}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitKind {
    Issue,
    Chapter,
    Volume,
    Special,
    Combined,
}

impl UnitKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Chapter => "chapter",
            Self::Volume => "volume",
            Self::Special => "special",
            Self::Combined => "combined",
        }
    }
    pub(crate) fn parse(value: String) -> Result<Self, String> {
        match value.as_str() {
            "issue" => Ok(Self::Issue),
            "chapter" => Ok(Self::Chapter),
            "volume" => Ok(Self::Volume),
            "special" => Ok(Self::Special),
            "combined" => Ok(Self::Combined),
            _ => Err(format!("invalid unit kind: {value}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatePrecision {
    Year,
    Month,
    Day,
}

impl DatePrecision {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
        }
    }
    pub(crate) fn parse(value: String) -> Result<Self, String> {
        match value.as_str() {
            "year" => Ok(Self::Year),
            "month" => Ok(Self::Month),
            "day" => Ok(Self::Day),
            _ => Err(format!("invalid date precision: {value}")),
        }
    }
}

pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub(crate) fn optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewPublication {
    pub content_type: ContentType,
    pub title: String,
    pub sort_title: Option<String>,
    pub run_label: Option<String>,
    pub known_unit_count: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationUpdate {
    pub title: Option<String>,
    pub sort_title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub run_label: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub known_unit_count: Option<Option<i64>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Publication {
    pub id: String,
    pub content_type: ContentType,
    pub title: String,
    pub sort_title: String,
    pub run_label: Option<String>,
    pub title_locked: bool,
    pub known_unit_count: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationSort {
    #[default]
    Title,
    RecentlyAdded,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    #[default]
    All,
    HasFiles,
    NoFiles,
}

#[derive(Clone, Debug, Default)]
pub struct PublicationFilter {
    pub kind: Option<ContentType>,
    pub q: Option<String>,
    pub sort: PublicationSort,
    pub availability: Availability,
}

/// Publication plus library availability. Monitor counts are scoped to the caller.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicationSummary {
    #[serde(flatten)]
    pub publication: Publication,
    pub file_count: i64,
    pub monitored_unit_count: i64,
    pub cover_file_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewEdition {
    pub publication_id: String,
    pub language: String,
    pub region: Option<String>,
    pub publisher: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditionUpdate {
    #[serde(default, deserialize_with = "optional")]
    pub language: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub region: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub publisher: Option<Option<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Edition {
    pub id: String,
    pub publication_id: String,
    pub language: String,
    pub region: Option<String>,
    pub publisher: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewUnit {
    pub edition_id: String,
    pub label: String,
    pub kind: UnitKind,
    pub sort_key: Option<String>,
    pub date: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnitUpdate {
    #[serde(default, deserialize_with = "optional")]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    pub kind: Option<UnitKind>,
    #[serde(default, deserialize_with = "present")]
    pub sort_key: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub date: Option<Option<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Unit {
    pub id: String,
    pub edition_id: String,
    pub label: String,
    pub kind: UnitKind,
    pub sort_key: Option<String>,
    pub date: Option<String>,
    pub date_precision: Option<DatePrecision>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewProviderLink {
    pub provider: String,
    pub external_id: String,
    pub publication_id: Option<String>,
    pub edition_id: Option<String>,
    pub unit_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderLink {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub publication_id: Option<String>,
    pub edition_id: Option<String>,
    pub unit_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
