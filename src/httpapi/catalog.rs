use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
};
use crate::{
    auth::Scope,
    catalog::{
        Availability, CatalogError, CatalogRepository, ContentType, EditionUpdate, NewEdition,
        NewProviderLink, NewPublication, NewUnit, PublicationFilter, PublicationSort,
        PublicationUpdate, UnitKind, UnitUpdate,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;

#[derive(Clone)]
pub struct CatalogContext {
    pub repository: CatalogRepository,
    pub auth: AuthContext,
}

pub fn routes(context: CatalogContext) -> Router {
    Router::new()
        .route("/api/v1/publications", get(list).post(create))
        .route(
            "/api/v1/publications/{id}",
            get(detail).patch(update).delete(remove),
        )
        .route("/api/v1/publications/{id}/editions", get(editions))
        .route("/api/v1/editions", post(create_edition))
        .route(
            "/api/v1/editions/{id}",
            axum::routing::patch(update_edition),
        )
        .route("/api/v1/editions/{id}/units", get(units))
        .route("/api/v1/units", post(create_unit))
        .route("/api/v1/units/{id}", axum::routing::patch(update_unit))
        .route("/api/v1/provider-links", post(create_provider_link))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(context)
}

async fn create_provider_link(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NewProviderLink>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok((
        StatusCode::CREATED,
        Json(context.repository.create_provider_link(input).await?),
    ))
}

impl From<CatalogError> for ApiError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Invalid(message) => Self::invalid(message),
            CatalogError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "conflict",
                "The operation conflicts with existing catalog data",
            ),
            CatalogError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Catalog record not found",
            ),
            CatalogError::Database(_) => Self::internal(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    q: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    // Accepted for compatibility; editions are not filtered by content type.
    #[allow(dead_code)]
    kind: Option<ContentType>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationQuery {
    q: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    kind: Option<ContentType>,
    #[serde(default)]
    sort: PublicationSort,
    #[serde(default)]
    availability: Availability,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitQuery {
    q: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    kind: Option<UnitKind>,
}

async fn list(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<PublicationQuery>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(Json(
        context
            .repository
            .list_publication_summaries(
                &principal.user_id,
                query.limit.unwrap_or(50),
                query.cursor.as_deref(),
                PublicationFilter {
                    kind: query.kind,
                    q: query.q,
                    sort: query.sort,
                    availability: query.availability,
                },
            )
            .await?,
    ))
}
async fn create(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NewPublication>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok((
        StatusCode::CREATED,
        Json(context.repository.create_publication(input).await?),
    ))
}
async fn detail(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(Json(
        context
            .repository
            .get_publication_summary(&id, &principal.user_id)
            .await?
            .ok_or_else(|| {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "Publication not found")
            })?,
    ))
}
async fn update(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<PublicationUpdate>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::PATCH, Scope::Manage).await?;
    Ok(Json(
        context.repository.update_publication(&id, input).await?,
    ))
}
async fn remove(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&context.auth, &headers, &Method::DELETE, Scope::Manage).await?;
    context.repository.delete_publication(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn editions(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(Json(
        context
            .repository
            .list_editions_page_search(
                &id,
                query.limit.unwrap_or(50),
                query.cursor.as_deref(),
                query.q.as_deref(),
            )
            .await?,
    ))
}
async fn create_edition(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NewEdition>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok((
        StatusCode::CREATED,
        Json(context.repository.create_edition(input).await?),
    ))
}
async fn update_edition(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<EditionUpdate>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::PATCH, Scope::Manage).await?;
    Ok(Json(context.repository.update_edition(&id, input).await?))
}
async fn units(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<UnitQuery>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(Json(
        context
            .repository
            .list_units_filtered(
                &id,
                query.limit.unwrap_or(50),
                query.cursor.as_deref(),
                query.q.as_deref(),
                query.kind,
            )
            .await?,
    ))
}
async fn create_unit(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NewUnit>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok((
        StatusCode::CREATED,
        Json(context.repository.create_unit(input).await?),
    ))
}
async fn update_unit(
    State(context): State<CatalogContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<UnitUpdate>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    authorize(&context.auth, &headers, &Method::PATCH, Scope::Manage).await?;
    Ok(Json(context.repository.update_unit(&id, input).await?))
}
