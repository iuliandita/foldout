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
    budget: Option<RequestBudget>,
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
            budget: None,
        })
    }

    pub(crate) fn with_budget(
        mut self,
        store: crate::store::sqlite::SqliteStore,
        scope: String,
    ) -> Self {
        self.budget = Some(RequestBudget { store, scope });
        self
    }
}

struct RequestBudget {
    store: crate::store::sqlite::SqliteStore,
    scope: String,
}

impl RequestBudget {
    async fn reserve(&self, deadline: tokio::time::Instant) -> Result<(), ProviderError> {
        loop {
            match crate::search::reserve_cooldown(&self.store, &self.scope, 1).await {
                Ok(()) => return Ok(()),
                Err(crate::search::SearchError::Cooldown {
                    retry_after_seconds,
                }) => {
                    let wait = Duration::from_secs(retry_after_seconds);
                    if wait >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
                        return Err(ProviderError::RateLimited {
                            retry_after_seconds: Some(retry_after_seconds),
                        });
                    }
                    tokio::time::sleep(wait).await;
                    // Waiting does not reserve a slot: another caller may have extended it.
                }
                Err(_) => return Err(ProviderError::Unavailable),
            }
        }
    }

    async fn limited(&self, seconds: Option<u64>) -> Result<(), ProviderError> {
        match crate::search::provider_result::<()>(
            &self.store,
            &self.scope,
            Err(ProviderError::RateLimited {
                retry_after_seconds: seconds,
            }),
        )
        .await
        {
            Err(crate::search::SearchError::Provider(error)) => Err(error),
            _ => Err(ProviderError::Unavailable),
        }
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

    pub(crate) async fn reserve(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), ProviderError> {
        if let Some(budget) = &self.config.budget {
            budget.reserve(deadline).await?;
        }
        Ok(())
    }

    pub(crate) async fn check_rate_limit(
        &self,
        response: &reqwest::Response,
    ) -> Result<(), ProviderError> {
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after);
            if let Some(budget) = &self.config.budget {
                budget.limited(seconds).await?;
            }
            return Err(ProviderError::RateLimited {
                retry_after_seconds: seconds,
            });
        }
        Ok(())
    }

    pub async fn execute(&self, request: RequestBuilder) -> Result<Response, ProviderError> {
        let deadline = tokio::time::Instant::now() + self.config.limits.timeout;
        tokio::time::timeout_at(deadline, async {
            self.reserve(deadline).await?;
            let mut response = request
                .send()
                .await
                .map_err(|_| ProviderError::Unavailable)?;
            self.check_rate_limit(&response).await?;
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

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::store::sqlite::SqliteStore;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn reservation_wait_obeys_http_deadline_without_sending() {
        let state = crate::search::mock::private_directory();
        let store = SqliteStore::open(state.path()).await.unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let requests = seen.clone();
        let app = axum::Router::new().fallback(move || {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                "owned"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(
            ProviderConfig::new(
                &base,
                None,
                HttpLimits {
                    timeout: Duration::from_millis(50),
                    ..HttpLimits::default()
                },
            )
            .unwrap()
            .with_budget(store.clone(), "owned".into()),
            false,
        )
        .unwrap();
        let writer = store.begin_write().await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            http.execute(http.get("").unwrap()),
        )
        .await;
        writer.commit().await.unwrap();
        task.abort();
        assert!(
            matches!(result, Ok(Err(ProviderError::Unavailable))),
            "HTTP deadline must include the database reservation wait"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_reservations_recheck_the_shared_slot() {
        let state = crate::search::mock::private_directory();
        let store = SqliteStore::open(state.path()).await.unwrap();
        let first = RequestBudget {
            store: store.clone(),
            scope: "owned".into(),
        };
        let second = RequestBudget {
            store: store.clone(),
            scope: "owned".into(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        let (a, b) = tokio::join!(first.reserve(deadline), second.reserve(deadline));
        a.unwrap();
        b.unwrap();
        let next: i64 =
            sqlx::query_scalar("SELECT next_at FROM search_cooldowns WHERE scope = 'owned'")
                .fetch_one(store.reader())
                .await
                .unwrap();
        assert!(next > crate::search::now());
    }

    #[tokio::test]
    async fn retry_after_survives_restart_and_blocks_http() {
        let state = crate::search::mock::private_directory();
        let store = SqliteStore::open(state.path()).await.unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let requests = seen.clone();
        let app = axum::Router::new().fallback(move || {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                axum::http::Response::builder()
                    .status(429)
                    .header("retry-after", "120")
                    .header("cf-mitigated", "challenge")
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(
            ProviderConfig::new(&base, None, HttpLimits::default())
                .unwrap()
                .with_budget(store.clone(), "owned".into()),
            false,
        )
        .unwrap();
        assert!(matches!(
            http.execute(http.get("").unwrap()).await,
            Err(ProviderError::RateLimited {
                retry_after_seconds: Some(120)
            })
        ));
        let before: i64 =
            sqlx::query_scalar("SELECT next_at FROM search_cooldowns WHERE scope = 'owned'")
                .fetch_one(store.reader())
                .await
                .unwrap();
        let budget = RequestBudget {
            store: store.clone(),
            scope: "owned".into(),
        };
        assert!(matches!(
            budget.limited(Some(1)).await,
            Err(ProviderError::RateLimited { .. })
        ));
        let after: i64 =
            sqlx::query_scalar("SELECT next_at FROM search_cooldowns WHERE scope = 'owned'")
                .fetch_one(store.reader())
                .await
                .unwrap();
        assert_eq!(
            before, after,
            "a shorter response must not reduce the cooldown"
        );
        drop(budget);
        drop(http);
        drop(store);
        let reopened = SqliteStore::open(state.path()).await.unwrap();
        let http = Http::new(
            ProviderConfig::new(&base, None, HttpLimits::default())
                .unwrap()
                .with_budget(reopened, "owned".into()),
            false,
        )
        .unwrap();
        assert!(matches!(
            http.execute(http.get("").unwrap()).await,
            Err(ProviderError::RateLimited { .. })
        ));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn unbudgeted_search_http_does_not_reserve_again() {
        let state = crate::search::mock::private_directory();
        let store = SqliteStore::open(state.path()).await.unwrap();
        crate::search::reserve_cooldown(&store, "owned", 120)
            .await
            .unwrap();
        let app = axum::Router::new().fallback(|| async { "owned" });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(
            ProviderConfig::new(&base, None, HttpLimits::default()).unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(
            http.execute(http.get("").unwrap()).await.unwrap().body,
            b"owned"
        );
        task.abort();
    }
}
