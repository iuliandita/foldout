use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
};
use crate::{
    acquisition::monitor::{CreateMonitor, Monitor, MonitorError, MonitorFilters, UpdateMonitor},
    auth::Scope,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;

#[derive(Clone)]
pub struct MonitorContext {
    pub monitor: Monitor,
    pub auth: AuthContext,
}
pub fn routes(context: MonitorContext) -> Router {
    Router::new()
        .route("/api/v1/monitors", get(list).post(create))
        .route("/api/v1/monitors/{id}", patch(update).delete(remove))
        .route("/api/v1/monitors/{id}/run", post(run))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(context)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default = "page_size")]
    limit: u32,
    cursor: Option<String>,
    publication_id: Option<String>,
    unit_id: Option<String>,
    enabled: Option<bool>,
}
fn page_size() -> u32 {
    20
}

/// DELETE uses the query string; POST run uses a JSON object with this shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorRevision {
    pub revision: i64,
}

async fn list(
    State(context): State<MonitorContext>,
    headers: HeaderMap,
    ApiQuery(page): ApiQuery<Page>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let page = context
        .monitor
        .list_filtered(
            &principal.user_id,
            page.limit,
            page.cursor.as_deref(),
            MonitorFilters {
                publication_id: page.publication_id,
                unit_id: page.unit_id,
                enabled: page.enabled,
            },
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response())
}
async fn create(
    State(context): State<MonitorContext>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CreateMonitor>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let item = context.monitor.create(&principal.user_id, request).await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(item),
    )
        .into_response())
}
async fn update(
    State(context): State<MonitorContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<UpdateMonitor>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::PATCH, Scope::Manage).await?;
    let item = context
        .monitor
        .update(&principal.user_id, &id, request)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(item)).into_response())
}
async fn remove(
    State(context): State<MonitorContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiQuery(request): ApiQuery<MonitorRevision>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::DELETE, Scope::Manage).await?;
    context
        .monitor
        .delete(&principal.user_id, &id, request.revision)
        .await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, "no-store")],
    )
        .into_response())
}
async fn run(
    State(context): State<MonitorContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<MonitorRevision>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let item = context
        .monitor
        .run(&principal.user_id, &id, request.revision)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(item),
    )
        .into_response())
}
impl From<MonitorError> for ApiError {
    fn from(error: MonitorError) -> Self {
        match error {
            MonitorError::Invalid => {
                Self::invalid("Invalid monitor scope, query, interval, revision, or pagination")
            }
            MonitorError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Monitor or catalog unit not found",
            ),
            MonitorError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "monitor_conflict",
                "Monitor revision changed, scope already exists, or monitor is disabled",
            ),
            MonitorError::Settings(error) => error.into(),
            MonitorError::Database => Self::internal(),
        }
    }
}
