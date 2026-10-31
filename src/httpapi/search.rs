use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
};
use crate::{
    auth::Scope,
    providers::ProviderError,
    search::{
        ArchiveAcceptRequest, ArchiveProposalError, ArchiveProposalRequest,
        ReleaseAssessmentRequest, ReleaseSearch, Search, SearchError,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

#[derive(Clone)]
pub struct SearchContext {
    pub search: Search,
    pub auth: AuthContext,
}

pub fn routes(context: SearchContext) -> Router {
    Router::new()
        .route("/api/v1/search/integrations", get(integrations))
        .route("/api/v1/search/metadata", get(metadata))
        .route("/api/v1/search/archive", get(archive))
        .route("/api/v1/search/archive/item", get(archive_item))
        .route("/api/v1/search/archive/proposal", get(archive_proposal))
        .route("/api/v1/search/archive/accept", post(archive_accept))
        .route("/api/v1/search/releases", post(releases))
        .route(
            "/api/v1/search/release-assessments",
            post(release_assessment),
        )
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(context)
}

async fn integrations(
    State(context): State<SearchContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"items":context.search.integrations().await?})),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataQuery {
    integration_id: String,
    query: String,
    #[serde(default = "first_page")]
    page: u32,
    #[serde(default = "page_size")]
    limit: u32,
}
fn first_page() -> u32 {
    1
}
fn page_size() -> u32 {
    20
}

async fn metadata(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<MetadataQuery>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    let page = context
        .search
        .metadata(&query.integration_id, &query.query, query.page, query.limit)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response())
}
async fn archive(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<MetadataQuery>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    let page = context
        .search
        .archive(&query.integration_id, &query.query, query.page, query.limit)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveItemQuery {
    integration_id: String,
    identifier: String,
}

async fn archive_item(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<ArchiveItemQuery>,
) -> Result<Response, ApiError> {
    authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await?;
    let item = context
        .search
        .archive_item(&query.integration_id, &query.identifier)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(item)).into_response())
}

async fn releases(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiJson(query): ApiJson<ReleaseSearch>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let page = context.search.releases(&principal.user_id, query).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response())
}

async fn release_assessment(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ReleaseAssessmentRequest>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let assessment = context
        .search
        .assess_release(&principal.user_id, request)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(assessment)).into_response())
}
impl From<SearchError> for ApiError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Invalid => Self::invalid("Invalid search query or pagination"),
            SearchError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "release_not_found",
                "Selected release is unavailable; search again",
            ),
            SearchError::UnitNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "unit_not_found",
                "Catalog unit was not found",
            ),
            SearchError::TargetChanged => Self::new(
                StatusCode::CONFLICT,
                "target_changed",
                "Catalog target details or selection policy changed; search again",
            ),
            SearchError::Unsupported => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported",
                "Integration does not support this operation",
            ),
            SearchError::Changed => Self::new(
                StatusCode::CONFLICT,
                "integration_changed",
                "Integration changed; search again",
            ),
            SearchError::ReleaseRejected => Self::new(
                StatusCode::CONFLICT,
                "release_rejected",
                "Release has an active rejection",
            ),
            SearchError::Cooldown { .. }
            | SearchError::Provider(ProviderError::RateLimited { .. }) => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "provider_cooldown",
                "Provider request cooldown is active",
            ),
            SearchError::Provider(ProviderError::InvalidQuery) => {
                Self::invalid("Invalid provider query")
            }
            SearchError::Provider(ProviderError::Unsupported) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported",
                "Provider capability is unavailable",
            ),
            SearchError::Provider(ProviderError::ChallengeRequired) => Self::new(
                StatusCode::CONFLICT,
                "challenge_required",
                "Provider requires user action",
            ),
            SearchError::Provider(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                "Provider could not complete this request",
            ),
            SearchError::Settings(error) => error.into(),
            SearchError::Database => Self::internal(),
        }
    }
}

async fn archive_proposal(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiQuery(request): ApiQuery<ArchiveProposalRequest>,
) -> Response {
    if let Err(error) = authorize(&context.auth, &headers, &Method::GET, Scope::Manage).await {
        return error.into_response();
    }
    match context.search.archive_unit_proposal(&request).await {
        Ok(proposal) => ([(header::CACHE_CONTROL, "no-store")], Json(proposal)).into_response(),
        Err(error) => archive_proposal_error(error),
    }
}

async fn archive_accept(
    State(context): State<SearchContext>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ArchiveAcceptRequest>,
) -> Response {
    let principal = match authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    match context
        .search
        .accept_archive_unit(&principal.user_id, request)
        .await
    {
        Ok(accepted) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(accepted),
        )
            .into_response(),
        Err(error) => archive_proposal_error(error),
    }
}

fn archive_proposal_error(error: ArchiveProposalError) -> Response {
    match error {
        ArchiveProposalError::Duplicate { existing_unit, proposal } => (
            StatusCode::CONFLICT,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({
                "error": { "code": "duplicate_unit", "message": "This Archive proposal matches an existing catalog unit", "trace_id": uuid::Uuid::new_v4().to_string() },
                "existing_unit_id": existing_unit.id,
                "existing_unit": existing_unit,
                "proposal": proposal,
            })),
        ).into_response(),
        ArchiveProposalError::Search(error) => ApiError::from(error).into_response(),
        ArchiveProposalError::Catalog(error) => ApiError::from(error).into_response(),
        ArchiveProposalError::IdempotencyConflict => ApiError::new(StatusCode::CONFLICT, "idempotency_conflict", "Idempotency key was already used with a different request").into_response(),
        ArchiveProposalError::Database(_) => ApiError::internal().into_response(),
    }
}
