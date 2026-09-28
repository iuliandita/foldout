use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::SettingsError;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IntegrationKind {
    ComicVine,
    MangaUpdates,
    MangaDex,
    InternetArchive,
    GetComics,
    Prowlarr,
    Sabnzbd,
    QBittorrent,
}

impl IntegrationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ComicVine => "comicvine",
            Self::MangaUpdates => "mangaupdates",
            Self::MangaDex => "mangadex",
            Self::InternetArchive => "internetarchive",
            Self::GetComics => "getcomics",
            Self::Prowlarr => "prowlarr",
            Self::Sabnzbd => "sabnzbd",
            Self::QBittorrent => "qbittorrent",
        }
    }
}

/// Write-only field: absent preserves, null clears, a string replaces.
#[derive(Default)]
pub enum SecretUpdate {
    #[default]
    Preserve,
    Clear,
    Set(String),
}

impl<'de> Deserialize<'de> for SecretUpdate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

impl SecretUpdate {
    pub(super) fn apply(self, value: &mut Option<String>) -> Result<(), SettingsError> {
        match self {
            Self::Preserve => {}
            Self::Clear => *value = None,
            Self::Set(secret) => {
                if secret.trim().is_empty()
                    || secret.len() > 4096
                    || secret.chars().any(char::is_control)
                {
                    return Err(SettingsError::Invalid(
                        "Credentials must be nonempty, at most 4096 bytes, and contain no control characters",
                    ));
                }
                *value = Some(secret);
            }
        }
        Ok(())
    }
}

fn empty_options() -> Value {
    serde_json::json!({})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIntegration {
    pub kind: IntegrationKind,
    pub label: String,
    pub base_url: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "empty_options")]
    pub options: Value,
    #[serde(default)]
    pub api_key: SecretUpdate,
    #[serde(default)]
    pub username: SecretUpdate,
    #[serde(default)]
    pub password: SecretUpdate,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateIntegration {
    #[serde(default, deserialize_with = "patch_value")]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "patch_value")]
    pub base_url: Option<String>,
    #[serde(default, deserialize_with = "patch_value")]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "patch_value")]
    pub options: Option<Value>,
    #[serde(default)]
    pub api_key: SecretUpdate,
    #[serde(default)]
    pub username: SecretUpdate,
    #[serde(default)]
    pub password: SecretUpdate,
}

fn patch_value<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("This field cannot be null"))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Usenet,
    Torrent,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryOptions {
    #[serde(default)]
    pub comics: Vec<u32>,
    #[serde(default)]
    pub manga: Vec<u32>,
    #[serde(default)]
    pub magazines: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProwlarrOptions {
    pub indexer_id: u32,
    pub protocol: Protocol,
    pub categories: CategoryOptions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOptions {
    pub category: String,
    pub remote_path: Option<String>,
    pub local_path: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyOptions {}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum IntegrationOptions {
    Provider(EmptyOptions),
    Prowlarr(ProwlarrOptions),
    Client(ClientOptions),
}

impl IntegrationOptions {
    pub(super) fn parse(kind: IntegrationKind, value: Value) -> Result<Self, SettingsError> {
        let invalid = || SettingsError::Invalid("Invalid options for this integration kind");
        match kind {
            IntegrationKind::Prowlarr => {
                let options: ProwlarrOptions =
                    serde_json::from_value(value).map_err(|_| invalid())?;
                if options.indexer_id == 0
                    || options.indexer_id > i32::MAX as u32
                    || [
                        &options.categories.comics,
                        &options.categories.manga,
                        &options.categories.magazines,
                    ]
                    .iter()
                    .any(|v| v.len() > 100 || v.contains(&0))
                {
                    return Err(invalid());
                }
                Ok(Self::Prowlarr(options))
            }
            IntegrationKind::Sabnzbd | IntegrationKind::QBittorrent => {
                let options: ClientOptions =
                    serde_json::from_value(value).map_err(|_| invalid())?;
                if options.category.is_empty()
                    || options.category.len() > 100
                    || !options
                        .category
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    || options.remote_path.is_some() != options.local_path.is_some()
                    || [&options.remote_path, &options.local_path].iter().any(|p| {
                        p.as_ref().is_some_and(|p| {
                            !std::path::Path::new(p).is_absolute()
                                || p.len() > 4096
                                || p.chars().any(char::is_control)
                                || std::path::Path::new(p)
                                    .components()
                                    .any(|c| c == std::path::Component::ParentDir)
                        })
                    })
                {
                    return Err(invalid());
                }
                Ok(Self::Client(options))
            }
            _ => Ok(Self::Provider(
                serde_json::from_value(value).map_err(|_| invalid())?,
            )),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Integration {
    pub id: String,
    pub kind: IntegrationKind,
    pub label: String,
    pub base_url: String,
    pub enabled: bool,
    pub options: IntegrationOptions,
    pub api_key_configured: bool,
    pub username_configured: bool,
    pub password_configured: bool,
    pub credentials_configured: bool,
}

pub(super) fn validate_label_url(label: &str, base_url: &str) -> Result<(), SettingsError> {
    if label.trim().is_empty() || label.len() > 100 || label.chars().any(char::is_control) {
        return Err(SettingsError::Invalid(
            "Label must contain 1 to 100 bytes without control characters",
        ));
    }
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| SettingsError::Invalid("Invalid integration base URL"))?;
    if base_url.len() > 2048
        || base_url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SettingsError::Invalid(
            "Base URL must be HTTP(S), without credentials, query, or fragment",
        ));
    }
    Ok(())
}
