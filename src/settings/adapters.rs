use crate::{clients, providers};

use super::{IntegrationKind, IntegrationOptions, Protocol, Settings, SettingsError};

/// Executable adapters contain secrets and deliberately implement neither Debug nor Serialize.
pub enum IntegrationAdapter {
    ComicVine(providers::ComicVine),
    MangaUpdates(providers::MangaUpdates),
    MangaDex(providers::MangaDex),
    InternetArchive(providers::internet_archive::InternetArchive),
    GetComics(providers::GetComics),
    Prowlarr(providers::Prowlarr),
    Sabnzbd(clients::Sabnzbd),
    QBittorrent(clients::QBittorrent),
}

impl Settings {
    /// Construct exclusively from saved admin settings. Adapters enforce no proxy/redirects.
    pub async fn adapter(&self, id: &str) -> Result<IntegrationAdapter, SettingsError> {
        let private = self.load_private(id).await?;
        let integration = private.integration;
        if !integration.enabled || !integration.credentials_configured {
            return Err(SettingsError::NotConfigured);
        }
        let invalid = |_| SettingsError::Invalid("Invalid integration adapter configuration");
        match integration.kind {
            IntegrationKind::Sabnzbd | IntegrationKind::QBittorrent => {
                let IntegrationOptions::Client(options) = integration.options else {
                    return Err(SettingsError::Database);
                };
                let config = clients::ClientConfig::new(
                    uuid::Uuid::parse_str(&integration.id).map_err(|_| SettingsError::Database)?,
                    &integration.base_url,
                    options.category,
                    clients::HttpLimits::default(),
                )
                .map_err(|_| SettingsError::Invalid("Invalid download client configuration"))?;
                match integration.kind {
                    IntegrationKind::Sabnzbd => Ok(IntegrationAdapter::Sabnzbd(
                        clients::Sabnzbd::new(
                            config,
                            private.api_key.ok_or(SettingsError::NotConfigured)?,
                        )
                        .map_err(|_| {
                            SettingsError::Invalid("Invalid download client credentials")
                        })?,
                    )),
                    _ => Ok(IntegrationAdapter::QBittorrent(
                        clients::QBittorrent::new(
                            config,
                            private.username.ok_or(SettingsError::NotConfigured)?,
                            private.password.ok_or(SettingsError::NotConfigured)?,
                        )
                        .map_err(|_| {
                            SettingsError::Invalid("Invalid download client credentials")
                        })?,
                    )),
                }
            }
            kind => {
                let config = providers::ProviderConfig::new(
                    &integration.base_url,
                    private.api_key,
                    providers::HttpLimits::default(),
                )
                .map_err(invalid)?;
                Ok(match kind {
                    IntegrationKind::ComicVine => IntegrationAdapter::ComicVine(
                        providers::ComicVine::new(config).map_err(invalid)?,
                    ),
                    IntegrationKind::MangaUpdates => IntegrationAdapter::MangaUpdates(
                        providers::MangaUpdates::new(config).map_err(invalid)?,
                    ),
                    IntegrationKind::MangaDex => IntegrationAdapter::MangaDex(
                        providers::MangaDex::new(config).map_err(invalid)?,
                    ),
                    IntegrationKind::InternetArchive => IntegrationAdapter::InternetArchive(
                        providers::internet_archive::InternetArchive::new(config)
                            .map_err(invalid)?,
                    ),
                    IntegrationKind::GetComics => IntegrationAdapter::GetComics(
                        providers::GetComics::new(config).map_err(invalid)?,
                    ),
                    IntegrationKind::Prowlarr => {
                        let IntegrationOptions::Prowlarr(options) = integration.options else {
                            return Err(SettingsError::Database);
                        };
                        let protocol = match options.protocol {
                            Protocol::Usenet => providers::ReleaseProtocol::Usenet,
                            Protocol::Torrent => providers::ReleaseProtocol::Torrent,
                        };
                        IntegrationAdapter::Prowlarr(
                            providers::Prowlarr::new(
                                config,
                                options.indexer_id,
                                protocol,
                                providers::CategoryMap {
                                    comics: options.categories.comics,
                                    manga: options.categories.manga,
                                    magazines: options.categories.magazines,
                                },
                            )
                            .map_err(invalid)?,
                        )
                    }
                    _ => return Err(SettingsError::Database),
                })
            }
        }
    }
}
