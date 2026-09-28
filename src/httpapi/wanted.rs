use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiQuery},
};
use crate::{
    auth::Scope,
    catalog::ContentType,
    catalog::wanted::{
        WantedAvailabilityFilter, WantedFilters, WantedMonitoring, WantedRepository,
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;

#[derive(Clone)]
pub struct WantedContext {
    pub repository: WantedRepository,
    pub auth: AuthContext,
}

pub fn routes(context: WantedContext) -> Router {
    Router::new()
        .route("/api/v1/wanted", get(list))
        .route("/api/v1/units/{id}", get(unit_context))
        .with_state(context)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    limit: Option<u32>,
    cursor: Option<String>,
    q: Option<String>,
    kind: Option<ContentType>,
    publication_id: Option<String>,
    monitoring: Option<WantedMonitoring>,
    availability: Option<WantedAvailabilityFilter>,
}

async fn list(
    State(context): State<WantedContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let page = context
        .repository
        .list(
            &principal.user_id,
            WantedFilters {
                q: query.q,
                kind: query.kind,
                publication_id: query.publication_id,
                monitoring: query.monitoring,
                availability: query.availability,
            },
            query.limit.unwrap_or(50),
            query.cursor.as_deref(),
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response())
}

async fn unit_context(
    State(context): State<WantedContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let unit = context.repository.unit_context(&id).await?.ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "Catalog unit not found")
    })?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(unit)).into_response())
}
