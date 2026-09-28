use super::{ClientError, ClientKind, OwnedJob, label};
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub struct HttpLimits {
    pub timeout: Duration,
    pub max_response_bytes: usize,
}
impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            max_response_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Administrator-trusted service root (including reverse-proxy prefix).
/// Never construct from a download URL or user-supplied request parameter.
pub struct ClientConfig {
    pub(super) id: Uuid,
    base: Url,
    pub(super) category: String,
    limits: HttpLimits,
}
impl ClientConfig {
    pub fn new(
        id: Uuid,
        trusted_base_url: &str,
        category: String,
        limits: HttpLimits,
    ) -> Result<Self, ClientError> {
        let mut base =
            Url::parse(trusted_base_url).map_err(|_| ClientError::InvalidConfiguration)?;
        if id.is_nil()
            || !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !label(&category)
            || limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(120)
            || limits.max_response_bytes == 0
            || limits.max_response_bytes > 16 * 1024 * 1024
        {
            return Err(ClientError::InvalidConfiguration);
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            id,
            base,
            category,
            limits,
        })
    }
}

pub(super) struct Http {
    pub config: ClientConfig,
    client: Client,
}
impl Http {
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .cookie_store(true)
            .timeout(config.limits.timeout)
            .connect_timeout(config.limits.timeout)
            .build()
            .map_err(|_| ClientError::InvalidConfiguration)?;
        Ok(Self { config, client })
    }
    pub fn request(&self, path: &str, post: bool) -> Result<RequestBuilder, ClientError> {
        let url = self
            .config
            .base
            .join(path)
            .map_err(|_| ClientError::InvalidConfiguration)?;
        let request = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        };
        Ok(request
            .header("Referer", self.config.base.as_str())
            .header("Origin", self.config.base.origin().ascii_serialization()))
    }
    pub async fn send(
        &self,
        request: RequestBuilder,
        mutation: bool,
    ) -> Result<Vec<u8>, ClientError> {
        self.read(request).await.map_err(|error| {
            if mutation {
                ClientError::NeedsReview
            } else {
                error
            }
        })
    }
    async fn read(&self, request: RequestBuilder) -> Result<Vec<u8>, ClientError> {
        let mut response = request.send().await.map_err(|_| ClientError::Unavailable)?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(ClientError::Authentication);
            }
            status if !status.is_success() => return Err(ClientError::Rejected),
            _ => {}
        }
        let max = self.config.limits.max_response_bytes;
        if response
            .content_length()
            .is_some_and(|size| size > max as u64)
        {
            return Err(ClientError::BodyTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ClientError::Unavailable)?
        {
            if chunk.len() > max.saturating_sub(bytes.len()) {
                return Err(ClientError::BodyTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    pub fn check(&self, job: &OwnedJob, kind: ClientKind) -> Result<(), ClientError> {
        if job.client_id != self.config.id
            || job.category != self.config.category
            || job.kind != kind
            || !super::external_valid(kind, &job.external_id)
        {
            return Err(ClientError::NotOwned);
        }
        Ok(())
    }
    pub fn receipt(
        &self,
        own_id: Uuid,
        kind: ClientKind,
        external_id: String,
    ) -> Result<OwnedJob, ClientError> {
        OwnedJob::from_persisted_receipt(
            own_id,
            self.config.id,
            kind,
            self.config.category.clone(),
            external_id,
        )
    }
}

pub(super) fn json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    serde_json::from_slice(bytes).map_err(|_| ClientError::InvalidResponse)
}
