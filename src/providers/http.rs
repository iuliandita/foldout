use super::ProviderError;
use reqwest::{Client, RequestBuilder, StatusCode, Url, header};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    pub timeout: Duration,
    pub max_body_bytes: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_body_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Trusted server configuration. Deliberately neither Debug nor Serialize.
pub struct ProviderConfig {
    base: Url,
    key: Option<String>,
    limits: HttpLimits,
}

impl ProviderConfig {
    /// The base path is retained: Comic Vine `/api/`, MangaUpdates `/v1/`,
    /// MangaDex `/`, Prowlarr its configured URL base, GetComics `/`.
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        limits: HttpLimits,
    ) -> Result<Self, ProviderError> {
        let mut base = Url::parse(base_url).map_err(|_| ProviderError::InvalidConfiguration)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(60)
            || limits.max_body_bytes == 0
            || limits.max_body_bytes > 4 * 1024 * 1024
            || api_key.as_ref().is_some_and(|k| {
                k.trim().is_empty() || k.len() > 4096 || k.chars().any(char::is_control)
            })
        {
            return Err(ProviderError::InvalidConfiguration);
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            base,
            key: api_key,
            limits,
        })
    }
}

pub(crate) struct Http {
    client: Client,
    config: ProviderConfig,
}
pub(crate) struct Response {
    pub status: StatusCode,
    pub challenge: bool,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Http {
    pub fn new(config: ProviderConfig, needs_key: bool) -> Result<Self, ProviderError> {
        if needs_key && config.key.is_none() {
            return Err(ProviderError::InvalidConfiguration);
        }
        let client = Client::builder()
            .user_agent("libraryd/0.1")
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.limits.timeout.min(Duration::from_secs(5)))
            .timeout(config.limits.timeout)
            .build()
            .map_err(|_| ProviderError::InvalidConfiguration)?;
        Ok(Self { client, config })
    }

    // Callers use only literal paths or numeric indexer IDs, never user URLs.
    pub fn get(&self, path: &str) -> Result<RequestBuilder, ProviderError> {
        let url = self
            .config
            .base
            .join(path)
            .map_err(|_| ProviderError::InvalidConfiguration)?;
        Ok(self.client.get(url))
    }
    pub fn post(&self, path: &str) -> Result<RequestBuilder, ProviderError> {
        let url = self
            .config
            .base
            .join(path)
            .map_err(|_| ProviderError::InvalidConfiguration)?;
        Ok(self.client.post(url))
    }
    pub fn key_query(&self, request: RequestBuilder, name: &str) -> RequestBuilder {
        match &self.config.key {
            Some(key) => request.query(&[(name, key)]),
            None => request,
        }
    }

    pub fn download_target(&self, value: &str) -> Result<Url, ProviderError> {
        if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
            return Err(ProviderError::InvalidResponse);
        }
        let url = self
            .config
            .base
            .join(value)
            .map_err(|_| ProviderError::InvalidResponse)?;
        if url.origin() != self.config.base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(ProviderError::Unsupported);
        }
        Ok(url)
    }

    pub async fn retrieve(&self, target: &Url) -> Result<Vec<u8>, ProviderError> {
        // Check again because the opaque descriptor may come from another adapter.
        let target = self.download_target(target.as_str())?;
        self.body(self.client.get(target)).await
    }

    /// Reject reflected credentials before constructing a public DTO. Download URLs
    /// are never parsed into returned objects. Never expose raw transport errors.
    pub fn text(&self, value: &str) -> Result<String, ProviderError> {
        let value = value.trim();
        if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
            return Err(ProviderError::InvalidResponse);
        }
        if let Some(key) = &self.config.key {
            let mut encoded = Url::parse("https://example.invalid/")
                .map_err(|_| ProviderError::InvalidConfiguration)?;
            encoded.query_pairs_mut().append_pair("k", key);
            let encoded = encoded
                .query()
                .and_then(|v| v.strip_prefix("k="))
                .unwrap_or(key);
            if value.contains(key)
                || value
                    .to_ascii_lowercase()
                    .contains(&encoded.to_ascii_lowercase())
            {
                return Err(ProviderError::InvalidResponse);
            }
        }
        if let Ok(url) = Url::parse(value)
            && (!url.username().is_empty()
                || url.password().is_some()
                || url.query_pairs().any(|(key, _)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "apikey" | "api_key" | "token" | "access_token" | "auth" | "password" | "r"
                    )
                }))
        {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(value.to_owned())
    }

    pub async fn execute(&self, request: RequestBuilder) -> Result<Response, ProviderError> {
        tokio::time::timeout(self.config.limits.timeout, async {
            let mut response = request
                .send()
                .await
                .map_err(|_| ProviderError::Unavailable)?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err(ProviderError::RateLimited {
                    retry_after_seconds: response
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(retry_after),
                });
            }
            let challenge = response
                .headers()
                .get("cf-mitigated")
                .is_some_and(|v| v == "challenge");
            if response.status().is_redirection() {
                return Err(ProviderError::Unavailable);
            }
            if response
                .content_length()
                .is_some_and(|len| len > self.config.limits.max_body_bytes as u64)
            {
                return Err(ProviderError::InvalidResponse);
            }
            let status = response.status();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ProviderError::Unavailable)?
            {
                if chunk.len() > self.config.limits.max_body_bytes - body.len() {
                    return Err(ProviderError::InvalidResponse);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Response {
                status,
                challenge,
                content_type,
                body,
            })
        })
        .await
        .map_err(|_| ProviderError::Unavailable)?
    }
    pub async fn body(&self, request: RequestBuilder) -> Result<Vec<u8>, ProviderError> {
        let response = self.execute(request).await?;
        if response.challenge {
            return Err(ProviderError::ChallengeRequired);
        }
        if !response.status.is_success() {
            return Err(ProviderError::Unavailable);
        }
        Ok(response.body)
    }
}

// Retry-After delta-seconds or IMF-fixdate. Malformed headers never reach DTOs.
fn retry_after(value: &str) -> Option<u64> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse().ok();
    }
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.len() != 6
        || !["Mon,", "Tue,", "Wed,", "Thu,", "Fri,", "Sat,", "Sun,"].contains(&parts[0])
        || parts[5] != "GMT"
    {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<u64> = parts[4]
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&year)
        || day < 1
        || day > days[(month - 1) as usize]
        || time.len() != 3
        || time[0] > 23
        || time[1] > 59
        || time[2] > 59
    {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y / 400;
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let epoch_days =
        era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * m + 2) / 5 + day - 1 - 719468;
    let timestamp = epoch_days as u64 * 86400 + time[0] * 3600 + time[1] * 60 + time[2];
    Some(timestamp.saturating_sub(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs()))
}
