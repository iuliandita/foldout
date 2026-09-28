use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiJson, ApiQuery},
    jobs::JobView,
};
use crate::{
    auth::Scope,
    importer::preview::{ImportPreview, PreviewError, PreviewService},
    jobs::Jobs,
    library::{
        LibraryFile, LibraryFormat,
        roots::{InventoryEntryPage, Library, LibraryError, Root},
    },
    store::sqlite::SqliteStore,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone)]
pub struct LibraryContext {
    pub library: Library,
    pub previews: PreviewService,
    pub jobs: Jobs,
    pub store: SqliteStore,
    pub auth: AuthContext,
}
pub fn routes(context: LibraryContext) -> Router {
    Router::new()
        .route("/api/v1/library/roots", get(roots).post(register))
        .route("/api/v1/library/roots/{id}/scan", post(scan))
        .route("/api/v1/library/roots/{id}/entries", get(entries))
        .route("/api/v1/library/previews", post(preview))
        .route("/api/v1/library/previews/{id}/accept", post(accept))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(context)
}
impl From<LibraryError> for ApiError {
    fn from(error: LibraryError) -> Self {
        match error {
            LibraryError::InvalidRoot => {
                Self::invalid("Root must be an existing accessible directory with a nonempty label")
            }
            LibraryError::RootNotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "Library root not found")
            }
            LibraryError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "root_conflict",
                "Library root already exists",
            ),
            LibraryError::Io(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "source_unavailable",
                "Library source could not be read safely",
            ),
            LibraryError::Database(_) => Self::internal(),
        }
    }
}
impl From<PreviewError> for ApiError {
    fn from(error: PreviewError) -> Self {
        match error {
            PreviewError::EntryNotFound | PreviewError::UnitNotFound | PreviewError::NotFound => {
                Self::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "Preview, inventory entry, or catalog unit not found",
                )
            }
            PreviewError::Stale => Self::new(
                StatusCode::CONFLICT,
                "stale_preview",
                "The source changed. Scan it again and review a new preview",
            ),
            PreviewError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "file_conflict",
                "A different file already owns this library path",
            ),
            PreviewError::UnsafeSource => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsafe_source",
                "Source could not be read safely",
            ),
            PreviewError::Database(_) => Self::internal(),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootInput {
    label: String,
    path: PathBuf,
}
async fn roots(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
) -> Result<Json<Vec<Root>>, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Admin).await?;
    Ok(Json(ctx.library.list_roots().await?))
}
async fn register(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<RootInput>,
) -> Result<(StatusCode, Json<Root>), ApiError> {
    authorize(&ctx.auth, &headers, &Method::POST, Scope::Admin).await?;
    if input.label.len() > 128 || input.path.as_os_str().len() > 4096 {
        return Err(ApiError::invalid("Root label or path is too long"));
    }
    Ok((
        StatusCode::CREATED,
        Json(ctx.library.register_root(&input.label, &input.path).await?),
    ))
}
async fn scan(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<JobView>), ApiError> {
    authorize(&ctx.auth, &headers, &Method::POST, Scope::Manage).await?;
    ctx.library.root(&id).await?;
    let job = ctx
        .jobs
        .enqueue(
            "library.scan",
            &format!("root:{id}"),
            serde_json::json!({"root_id":id}),
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(job.into())))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryQuery {
    limit: Option<u32>,
    cursor: Option<String>,
}
async fn entries(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<EntryQuery>,
) -> Result<Json<InventoryEntryPage>, ApiError> {
    authorize(&ctx.auth, &headers, &Method::GET, Scope::Manage).await?;
    ctx.library.root(&id).await?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::invalid("Limit must be 1 to 100"));
    }
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| uuid::Uuid::parse_str(cursor).is_err())
    {
        return Err(ApiError::invalid("Invalid inventory cursor"));
    }
    Ok(Json(
        ctx.library
            .inventory_entries(&id, query.cursor.as_deref(), limit)
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewInput {
    entry_id: String,
    unit_id: String,
}
async fn preview(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<PreviewInput>,
) -> Result<(StatusCode, Json<ImportPreview>), ApiError> {
    authorize(&ctx.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            ctx.previews
                .preview(&input.entry_id, &input.unit_id)
                .await?,
        ),
    ))
}
#[derive(Serialize)]
pub struct FileView {
    pub id: String,
    pub format: LibraryFormat,
    pub signature: String,
    pub size_bytes: i64,
}
impl From<LibraryFile> for FileView {
    fn from(file: LibraryFile) -> Self {
        Self {
            id: file.id,
            format: file.format,
            signature: file.signature,
            size_bytes: file.size_bytes,
        }
    }
}
async fn accept(
    State(ctx): State<LibraryContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<FileView>, ApiError> {
    authorize(&ctx.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(Json(ctx.previews.accept(&id).await?.into()))
}
