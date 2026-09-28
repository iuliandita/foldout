use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson},
};
use crate::{
    auth::Scope,
    settings::{CreateIntegration, Settings, SettingsError, UpdateIntegration},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};

#[derive(Clone)]
pub struct SettingsContext {
    pub settings: Settings,
    pub auth: AuthContext,
}

pub fn routes(context: SettingsContext) -> Router {
    Router::new()
        .route("/api/v1/settings/integrations", get(list).post(create))
        .route(
            "/api/v1/settings/integrations/{id}",
            patch(update).delete(remove),
        )
        .route(
            "/api/v1/settings/integrations/{id}/test",
            post(test_connection),
        )
        .layer(DefaultBodyLimit::max(32 * 1024))
        .with_state(context)
}

static TEST_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

async fn test_connection(
    State(context): State<SettingsContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Admin).await?;
    let _slot = TEST_SLOT.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "test_in_progress",
            "An integration check is already running",
        )
    })?;
    use crate::settings::IntegrationAdapter;
    let mut version = None;
    let mut capabilities = None;
    match context.settings.adapter(&id).await? {
        IntegrationAdapter::Sabnzbd(client) => {
            version = Some(
                client
                    .test_connection()
                    .await
                    .map_err(client_error)?
                    .version,
            )
        }
        IntegrationAdapter::QBittorrent(client) => {
            version = Some(
                client
                    .test_connection()
                    .await
                    .map_err(client_error)?
                    .version,
            )
        }
        IntegrationAdapter::Prowlarr(provider) => {
            capabilities = Some(
                serde_json::to_value(provider.capabilities().await.map_err(provider_error)?)
                    .map_err(|_| ApiError::internal())?,
            )
        }
        IntegrationAdapter::GetComics(provider) => {
            capabilities = Some(
                serde_json::to_value(provider.capability().await.map_err(provider_error)?)
                    .map_err(|_| ApiError::internal())?,
            )
        }
        IntegrationAdapter::ComicVine(provider) => {
            provider
                .search("test", crate::providers::SearchPage { number: 1, size: 1 })
                .await
                .map_err(provider_error)?;
        }
        IntegrationAdapter::MangaUpdates(provider) => {
            provider
                .search("test", crate::providers::SearchPage { number: 1, size: 1 })
                .await
                .map_err(provider_error)?;
        }
        IntegrationAdapter::MangaDex(provider) => {
            provider
                .search("test", crate::providers::SearchPage { number: 1, size: 1 })
                .await
                .map_err(provider_error)?;
        }
        IntegrationAdapter::InternetArchive(provider) => {
            provider
                .search(
                    "magazine",
                    crate::providers::SearchPage { number: 1, size: 1 },
                )
                .await
                .map_err(provider_error)?;
        }
    }
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"status":"ok","version":version,"capabilities":capabilities})),
    )
        .into_response())
}

fn client_error(error: crate::clients::ClientError) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "client_check_failed",
        error.to_string(),
    )
}
fn provider_error(error: crate::providers::ProviderError) -> ApiError {
    let (status, code) = match error {
        crate::providers::ProviderError::RateLimited { .. } => {
            (StatusCode::TOO_MANY_REQUESTS, "provider_cooldown")
        }
        crate::providers::ProviderError::ChallengeRequired => {
            (StatusCode::CONFLICT, "challenge_required")
        }
        _ => (StatusCode::BAD_GATEWAY, "provider_check_failed"),
    };
    ApiError::new(status, code, error.to_string())
}

impl From<SettingsError> for ApiError {
    fn from(error: SettingsError) -> Self {
        match error {
            SettingsError::Invalid(message) => Self::invalid(message),
            SettingsError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "Integration not found")
            }
            SettingsError::NotConfigured => Self::new(
                StatusCode::CONFLICT,
                "not_configured",
                "Integration is disabled or lacks required credentials",
            ),
            SettingsError::Database
            | SettingsError::Encryption
            | SettingsError::KeyUnavailable
            | SettingsError::MissingKey => Self::internal(),
        }
    }
}

async fn list(
    State(context): State<SettingsContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Admin).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"items": context.settings.list().await?})),
    )
        .into_response())
}

async fn create(
    State(context): State<SettingsContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<CreateIntegration>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Admin).await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(context.settings.create(input).await?),
    )
        .into_response())
}

async fn update(
    State(context): State<SettingsContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<UpdateIntegration>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::PATCH, Scope::Admin).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(context.settings.update(&id, input).await?),
    )
        .into_response())
}

async fn remove(
    State(context): State<SettingsContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&context.auth, &headers, &Method::DELETE, Scope::Admin).await?;
    context.settings.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
