use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
};
use crate::{
    acquisition::direct::{Direct, DirectChapterSearch, DirectError, DirectRequest, DirectSearch},
    auth::Scope,
    providers::ProviderError,
    settings::Settings,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

#[derive(Clone)]
pub struct DirectContext {
    pub direct: Direct,
    pub settings: Settings,
    pub auth: AuthContext,
}

pub fn routes(context: DirectContext) -> Router {
    Router::new()
        .route("/api/v1/direct/search", post(search))
        .route("/api/v1/direct/chapters", post(chapters))
        .route("/api/v1/direct/details", post(detail))
        .route("/api/v1/direct/resolve", post(resolve))
        .route("/api/v1/direct/acquisitions", get(list).post(create))
        .route("/api/v1/direct/acquisitions/{id}", get(status))
        .route("/api/v1/direct/acquisitions/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(context)
}
fn response<T: serde::Serialize>(value: T) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}
async fn chapters(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<DirectChapterSearch>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(response(
        context
            .direct
            .chapters(&context.settings, &principal.user_id, input)
            .await?,
    ))
}
async fn search(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<DirectSearch>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(response(
        context
            .direct
            .search(&context.settings, &principal.user_id, input)
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostSelection {
    post_handle: String,
}
async fn detail(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<PostSelection>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(response(
        context
            .direct
            .detail(&context.settings, &principal.user_id, &input.post_handle)
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSelection {
    link_handle: String,
}
async fn resolve(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<LinkSelection>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(response(
        context
            .direct
            .resolve(&context.settings, &principal.user_id, &input.link_handle)
            .await?,
    ))
}
async fn create(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<DirectRequest>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let mut keys = headers.get_all("idempotency-key").iter();
    let key = keys
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::invalid("Idempotency-Key header is required"))?;
    if keys.next().is_some() {
        return Err(ApiError::invalid("Use one Idempotency-Key header"));
    }
    Ok((
        StatusCode::ACCEPTED,
        response(
            context
                .direct
                .create(&context.settings, &principal.user_id, key, input)
                .await?,
        ),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectCancel {}
async fn cancel(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(_input): ApiJson<DirectCancel>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(response(
        context.direct.cancel(&principal.user_id, &id).await?,
    ))
}
async fn status(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    Ok(response(context.direct.get(&principal.user_id, &id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}
fn default_limit() -> u32 {
    20
}
async fn list(
    State(context): State<DirectContext>,
    headers: HeaderMap,
    ApiQuery(page): ApiQuery<Page>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    Ok(response(
        serde_json::json!({"items":context.direct.list(&principal.user_id,page.limit,page.offset).await?}),
    ))
}
impl From<DirectError> for ApiError {
    fn from(error: DirectError) -> Self {
        match error {
            DirectError::Invalid | DirectError::Provider(ProviderError::InvalidQuery) => {
                Self::invalid("Invalid direct acquisition request")
            }
            DirectError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Selection or acquisition was not found",
            ),
            DirectError::Busy => Self::new(
                StatusCode::CONFLICT,
                "direct_busy",
                "Direct worker is busy; try cancellation later",
            ),
            DirectError::ReviewRequired => Self::new(
                StatusCode::CONFLICT,
                "direct_review_required",
                "Intent requires import or local review before cancellation",
            ),
            DirectError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "direct_conflict",
                "Selection, unit, destination, or idempotency key conflicts",
            ),
            DirectError::Changed => Self::new(
                StatusCode::CONFLICT,
                "source_changed",
                "Source changed; select the link again",
            ),
            DirectError::ManualAction => Self::new(
                StatusCode::CONFLICT,
                "manual_action",
                "Selected mirror requires manual action",
            ),
            DirectError::Provider(ProviderError::ChallengeRequired) => Self::new(
                StatusCode::CONFLICT,
                "challenge_required",
                "Provider requires user action",
            ),
            DirectError::Provider(ProviderError::Unsupported) | DirectError::NetworkPolicy => {
                Self::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported",
                    "Selected download is unsupported by the direct host policy",
                )
            }
            DirectError::Provider(ProviderError::RateLimited { .. }) => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Source request rate limit is active",
            ),
            DirectError::Settings(error) => error.into(),
            DirectError::LocalReview => Self::new(
                StatusCode::CONFLICT,
                "local_review",
                "Registered root or file requires local review",
            ),
            DirectError::Database => Self::internal(),
            _ => Self::new(
                StatusCode::BAD_GATEWAY,
                "source_unavailable",
                "Direct source could not complete the operation",
            ),
        }
    }
}
