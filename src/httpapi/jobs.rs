use super::{
    auth::{AuthContext, authorize},
    errors::{ApiError, ApiQuery},
};
use crate::{
    auth::{Principal, Scope},
    jobs::{Job, JobError, JobState, Jobs},
    library::roots::Library,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct JobContext {
    pub jobs: Jobs,
    pub library: Library,
    pub auth: AuthContext,
}

pub fn routes(context: JobContext) -> Router {
    Router::new()
        .route("/api/v1/jobs", get(list))
        .route("/api/v1/jobs/{id}", get(detail))
        .route("/api/v1/jobs/{id}/cancel", post(cancel))
        .route("/api/v1/jobs/{id}/retry", post(retry))
        .with_state(context)
}

#[derive(Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub state: JobState,
    pub attempts: i64,
    pub retry_at: Option<i64>,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub subject: Option<JobSubject>,
}

/// What a job operates on. Root labels are shown only to admins, who can list roots anyway;
/// root paths are never exposed here.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSubject {
    LibraryRoot {
        root_id: String,
        root_label: Option<String>,
    },
}

impl From<Job> for JobView {
    fn from(job: Job) -> Self {
        let subject = (job.kind == "library.scan")
            .then(|| {
                job.payload
                    .get("root_id")
                    .and_then(serde_json::Value::as_str)
            })
            .flatten()
            .map(|root_id| JobSubject::LibraryRoot {
                root_id: root_id.to_owned(),
                root_label: None,
            });
        Self {
            subject,
            id: job.id,
            kind: job.kind,
            state: job.state,
            attempts: job.attempts,
            retry_at: job.retry_at,
            reason: job.reason,
            created_at: job.created_at,
            updated_at: job.updated_at,
        }
    }
}
/// Adds root labels for admins; other scopes see only the opaque root id.
async fn label_subjects(
    library: &Library,
    principal: &Principal,
    views: &mut [JobView],
) -> Result<(), ApiError> {
    if principal.scope != Scope::Admin
        || !views
            .iter()
            .any(|view| matches!(view.subject, Some(JobSubject::LibraryRoot { .. })))
    {
        return Ok(());
    }
    let labels: std::collections::HashMap<String, String> = library
        .list_roots()
        .await?
        .into_iter()
        .map(|root| (root.id, root.label))
        .collect();
    for view in views {
        if let Some(JobSubject::LibraryRoot {
            root_id,
            root_label,
        }) = &mut view.subject
        {
            *root_label = labels.get(root_id).cloned();
        }
    }
    Ok(())
}

impl From<JobError> for ApiError {
    fn from(error: JobError) -> Self {
        match error {
            JobError::Invalid(message) => Self::invalid(message),
            JobError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "job_conflict",
                "The job state no longer permits this operation",
            ),
            JobError::NotFound => Self::new(StatusCode::NOT_FOUND, "not_found", "Job not found"),
            JobError::Database(_) => Self::internal(),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    limit: Option<usize>,
    cursor: Option<String>,
}
#[derive(Serialize)]
struct Page {
    items: Vec<JobView>,
    next_cursor: Option<String>,
}
async fn list(
    State(context): State<JobContext>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<Query>,
) -> Result<Json<Page>, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let page = context
        .jobs
        .list_page(query.limit.unwrap_or(50), query.cursor.as_deref())
        .await?;
    let mut items: Vec<JobView> = page.items.into_iter().map(JobView::from).collect();
    label_subjects(&context.library, &principal, &mut items).await?;
    Ok(Json(Page {
        items,
        next_cursor: page.next_cursor,
    }))
}
async fn detail(
    State(context): State<JobContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let mut view = [JobView::from(
        context.jobs.get(&id).await?.ok_or(JobError::NotFound)?,
    )];
    label_subjects(&context.library, &principal, &mut view).await?;
    let [view] = view;
    Ok(Json(view))
}
async fn cancel(
    State(context): State<JobContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(Json(context.jobs.request_cancel(&id).await?.into()))
}
async fn retry(
    State(context): State<JobContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    authorize(&context.auth, &headers, &Method::POST, Scope::Manage).await?;
    Ok(Json(context.jobs.retry_reviewed(&id).await?.into()))
}
