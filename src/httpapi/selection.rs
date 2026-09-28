use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson},
};
use crate::{
    auth::Scope,
    search::selection::{NewDecision, SelectionError, SelectionRepository},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;

#[derive(Clone)]
pub struct SelectionContext {
    pub repository: SelectionRepository,
    pub auth: AuthContext,
}

pub fn routes(context: SelectionContext) -> Router {
    Router::new()
        .route("/api/v1/search/release-decisions", post(decide))
        .route(
            "/api/v1/search/release-decisions/{id}/revocations",
            post(revoke),
        )
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(context)
}

async fn decide(
    State(context): State<SelectionContext>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<NewDecision>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let decision = context
        .repository
        .decide(&principal.user_id, idempotency_key(&headers)?, request)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(decision)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

async fn revoke(
    State(context): State<SelectionContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(Empty {}): ApiJson<Empty>,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    let revocation = context
        .repository
        .revoke_rejection(&principal.user_id, idempotency_key(&headers)?, &id)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(revocation)).into_response())
}

fn idempotency_key(headers: &HeaderMap) -> Result<&str, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let key = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::invalid("Idempotency-Key header is required"))?;
    if values.next().is_some() {
        return Err(ApiError::invalid("Use a single Idempotency-Key header"));
    }
    Ok(key)
}

impl From<SelectionError> for ApiError {
    fn from(error: SelectionError) -> Self {
        match error {
            SelectionError::Invalid => Self::invalid("Invalid release selection request"),
            SelectionError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "selection_not_found",
                "Selection record was not found",
            ),
            SelectionError::Changed => Self::new(
                StatusCode::CONFLICT,
                "assessment_changed",
                "Assessment is no longer current",
            ),
            SelectionError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "selection_conflict",
                "Selection conflicts with an existing action",
            ),
            SelectionError::Expired => Self::new(
                StatusCode::CONFLICT,
                "assessment_expired",
                "Assessment has expired",
            ),
            SelectionError::Ineligible => Self::new(
                StatusCode::CONFLICT,
                "release_ineligible",
                "Release conflicts with the selected unit",
            ),
            SelectionError::AcknowledgementRequired => Self::new(
                StatusCode::CONFLICT,
                "assessment_acknowledgement_required",
                "Acknowledge this assessment before selecting the release",
            ),
            SelectionError::Rejected => Self::new(
                StatusCode::CONFLICT,
                "release_rejected",
                "Release has an active rejection",
            ),
            SelectionError::Superseded => Self::new(
                StatusCode::CONFLICT,
                "selection_superseded",
                "A later rejection superseded this selection; select the release again",
            ),
            SelectionError::Database => Self::internal(),
        }
    }
}
