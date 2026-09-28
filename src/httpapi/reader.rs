use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson},
};
use crate::{
    auth::Scope,
    reader::service::{ProgressUpdate, ReaderError, ReaderService},
};

#[derive(Clone)]
pub struct ReaderContext {
    pub service: ReaderService,
    pub auth: AuthContext,
}

pub fn routes(context: ReaderContext) -> Router {
    Router::new()
        .route("/api/v1/library/files/{id}/manifest", get(manifest))
        .route("/api/v1/library/files/{id}/metadata", get(metadata))
        .route("/api/v1/library/files/{id}/context", get(file_context))
        .route("/api/v1/library/files/{id}/pages/{page}", get(page))
        .route("/api/v1/library/files/{id}/thumbnail", get(thumbnail))
        .route(
            "/api/v1/library/files/{id}/progress",
            get(progress).put(save_progress),
        )
        .route("/api/v1/units/{id}/files", get(files))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(context)
}

impl From<ReaderError> for ApiError {
    fn from(error: ReaderError) -> Self {
        let (status, code) = match &error {
            ReaderError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "reader_busy"),
            ReaderError::Timeout => (StatusCode::SERVICE_UNAVAILABLE, "reader_timeout"),
            ReaderError::EncryptedDocument => {
                (StatusCode::UNPROCESSABLE_ENTITY, "encrypted_document")
            }
            ReaderError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ReaderError::UnsafeSource => (StatusCode::UNPROCESSABLE_ENTITY, "unsafe_source"),
            ReaderError::StaleSource => (StatusCode::CONFLICT, "stale_source"),
            ReaderError::InvalidDocument => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_document"),
            ReaderError::PageBounds => (StatusCode::BAD_REQUEST, "page_out_of_bounds"),
            ReaderError::RevisionConflict => (StatusCode::CONFLICT, "revision_conflict"),
            ReaderError::ResetRequired => (StatusCode::CONFLICT, "progress_reset_required"),
            ReaderError::Database(_) => return Self::internal(),
        };
        Self::new(status, code, error.to_string())
    }
}

fn json(value: impl serde::Serialize) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

async fn manifest(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(json(ctx.service.manifest(&id).await?))
}

async fn metadata(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Manage).await?;
    Ok(json(
        serde_json::json!({"file_id":id,"hints":ctx.service.metadata(&id).await?}),
    ))
}

async fn file_context(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(json(
        serde_json::json!({"items": ctx.service.context(&id).await?}),
    ))
}

async fn page(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path((id, page)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    let page = page
        .parse::<usize>()
        .map_err(|_| ApiError::invalid("Page must be a zero-based integer"))?;
    if page >= 10_000 {
        return Err(ReaderError::PageBounds.into());
    }
    let page = ctx.service.page(&id, page).await?;
    Ok((
        [
            (header::CONTENT_TYPE, page.content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        page.bytes,
    )
        .into_response())
}

const THUMBNAIL_CACHE: &str = "private, max-age=86400";

fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|tag| tag.trim())
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
}

async fn thumbnail(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    let etag = format!("\"{}\"", ctx.service.file_signature(&id).await?);
    if matches_etag(&headers, &etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, THUMBNAIL_CACHE.into()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            ],
        )
            .into_response());
    }
    let thumbnail = ctx.service.thumbnail(&id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg".to_owned()),
            (header::ETAG, format!("\"{}\"", thumbnail.signature)),
            (header::CACHE_CONTROL, THUMBNAIL_CACHE.into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        thumbnail.bytes,
    )
        .into_response())
}

async fn progress(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let principal = authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(json(ctx.service.progress(&principal.user_id, &id).await?))
}

async fn save_progress(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ProgressUpdate>,
) -> Result<Response, ApiError> {
    let principal = authorize(&ctx.auth, &headers, &Method::PUT, Scope::Manage).await?;
    Ok(json(
        ctx.service
            .save_progress(&principal.user_id, &id, input)
            .await?,
    ))
}

async fn files(
    State(ctx): State<ReaderContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Read).await?;
    Ok(json(
        serde_json::json!({"items": ctx.service.files(&id).await?}),
    ))
}
