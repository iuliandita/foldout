use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub listen: SocketAddr,
    pub origin: String,
    pub workers: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("state directory must not be empty")]
    EmptyStateDir,
    #[error("{0} must be valid Unicode")]
    InvalidEnvironment(&'static str),
    #[error("LIBRARY_ORIGIN must be an HTTP(S) origin without credentials, query, or path")]
    InvalidOrigin,
    #[error("LIBRARY_WORKERS must be true or false")]
    InvalidWorkers,
    #[error("invalid listen address: {0}")]
    InvalidListenAddress(#[source] std::net::AddrParseError),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let state_dir = match env::var("LIBRARY_STATE_DIR") {
            Ok(value) => PathBuf::from(value),
            Err(env::VarError::NotPresent) => PathBuf::from("./state"),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::InvalidEnvironment("LIBRARY_STATE_DIR"));
            }
        };
        let listen = match env::var("LIBRARY_LISTEN") {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => "127.0.0.1:8787".to_owned(),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::InvalidEnvironment("LIBRARY_LISTEN"));
            }
        };

        let mut config = Self::parse(state_dir, &listen)?;
        match env::var("LIBRARY_ORIGIN") {
            Ok(origin) => config.origin = Self::validate_origin(&origin)?,
            Err(env::VarError::NotPresent) => {}
            Err(_) => return Err(ConfigError::InvalidEnvironment("LIBRARY_ORIGIN")),
        }
        config.workers = match env::var("LIBRARY_WORKERS") {
            Ok(value) => parse_workers(&value)?,
            Err(env::VarError::NotPresent) => true,
            Err(_) => return Err(ConfigError::InvalidEnvironment("LIBRARY_WORKERS")),
        };
        Ok(config)
    }

    pub fn parse(state_dir: PathBuf, listen: &str) -> Result<Self, ConfigError> {
        if state_dir.as_os_str().is_empty() {
            return Err(ConfigError::EmptyStateDir);
        }

        Ok(Self {
            state_dir,
            listen: listen.parse().map_err(ConfigError::InvalidListenAddress)?,
            origin: format!("http://{listen}"),
            workers: true,
        })
    }

    pub fn validate_origin(origin: &str) -> Result<String, ConfigError> {
        let uri: axum::http::Uri = origin.parse().map_err(|_| ConfigError::InvalidOrigin)?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri
                .authority()
                .is_none_or(|value| value.as_str().contains('@'))
            || !matches!(uri.path(), "" | "/")
            || uri.query().is_some()
            || origin.contains('#')
        {
            return Err(ConfigError::InvalidOrigin);
        }
        Ok(origin.trim_end_matches('/').to_string())
    }
}

fn parse_workers(value: &str) -> Result<bool, ConfigError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigError::InvalidWorkers),
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
