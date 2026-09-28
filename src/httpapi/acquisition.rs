use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
};
use crate::{
    acquisition::pipeline::{AcquisitionRequest, FileAssociation, Pipeline, PipelineError},
    auth::Scope,
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
pub struct AcquisitionContext {
    pub pipeline: Pipeline,
    pub settings: Settings,
    pub auth: AuthContext,
}

pub fn routes(context: AcquisitionContext) -> Router {
    Router::new()
        .route("/api/v1/acquisition", get(list).post(create))
        .route("/api/v1/acquisition/roots", get(roots))
        .route("/api/v1/acquisition/{id}", get(detail))
        .route("/api/v1/acquisition/{id}/files", post(associate_file))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(context)
}

async fn roots(
    State(context): State<AcquisitionContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"items": context.pipeline.roots().await?})),
    )
        .into_response())
}
async fn create(
    State(context): State<AcquisitionContext>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<AcquisitionRequest>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let mut values = headers.get_all("idempotency-key").iter();
    let key = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::invalid("Idempotency-Key header is required"))?;
    if values.next().is_some() {
        return Err(ApiError::invalid("Use a single Idempotency-Key header"));
    }
    let acquisition = context
        .pipeline
        .create(&context.settings, &principal.user_id, key, request)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(acquisition),
    )
        .into_response())
}
async fn detail(
    State(context): State<AcquisitionContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(context.pipeline.get(&principal.user_id, &id).await?),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default = "page_size")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}
fn page_size() -> u32 {
    20
}
async fn list(
    State(context): State<AcquisitionContext>,
    headers: HeaderMap,
    ApiQuery(page): ApiQuery<Page>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let items = context
        .pipeline
        .list(&principal.user_id, page.limit, page.offset)
        .await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"items": items})),
    )
        .into_response())
}
async fn associate_file(
    State(context): State<AcquisitionContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<FileAssociation>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let acquisition = context
        .pipeline
        .associate_file(&principal.user_id, &id, request)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(acquisition),
    )
        .into_response())
}
impl From<PipelineError> for ApiError {
    fn from(error: PipelineError) -> Self {
        match error {
            PipelineError::Invalid => Self::invalid("Invalid acquisition selection"),
            PipelineError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Acquisition or selection not found",
            ),
            PipelineError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "acquisition_conflict",
                "Selection, destination, or idempotency key conflicts with an existing intent",
            ),
            PipelineError::Search(error) => error.into(),
            PipelineError::Settings(error) => error.into(),
            PipelineError::Selection(error) => error.into(),
            PipelineError::Database => Self::internal(),
        }
    }
}
