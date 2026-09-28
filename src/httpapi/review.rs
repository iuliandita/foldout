//! Read-only review feed. Each source keeps the access rule of the endpoint that owns it:
//! indexer acquisitions are caller-scoped (Read), direct acquisitions are owner-scoped
//! (Manage), inventory entries require Manage, jobs are unscoped (Read), and monitors are
//! owner-scoped (Read). Sources the credential cannot read report a null total.
use super::{
    auth::{AuthContext, authorize},
    errors::ApiError,
};
use crate::{auth::Scope, store::sqlite::SqliteStore};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use sqlx::{Row, sqlite::SqliteRow};

const PER_KIND: i64 = 100;

#[derive(Clone)]
pub struct ReviewContext {
    pub store: SqliteStore,
    pub auth: AuthContext,
}

pub fn routes(context: ReviewContext) -> Router {
    Router::new()
        .route("/api/v1/review", get(feed))
        .with_state(context)
}

#[derive(Debug, Serialize)]
pub struct ReviewItem {
    pub kind: &'static str,
    pub id: String,
    pub created_at: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
    pub publication_id: Option<String>,
    pub unit_id: Option<String>,
    pub acquisition_id: Option<String>,
    pub job_id: Option<String>,
    pub job_kind: Option<String>,
    pub root_id: Option<String>,
    pub entry_id: Option<String>,
    pub relative_path: Option<String>,
    pub monitor_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReviewTotals {
    pub acquisition_needs_review: i64,
    pub direct_acquisition_needs_review: Option<i64>,
    pub file_to_link: Option<i64>,
    pub job_failed: i64,
    pub monitor_needs_review: i64,
}

#[derive(Debug, Serialize)]
pub struct ReviewFeed {
    pub items: Vec<ReviewItem>,
    pub totals: ReviewTotals,
}

// Runs in needs_review (including uncertain_submission) or whose job stopped for review.
macro_rules! acquisition_filter {
    () => {
        "FROM acquisition_runs a
    JOIN acquisition_intents i ON i.id = a.id
    JOIN jobs j ON j.id = i.job_id
    JOIN units u ON u.id = a.unit_id
    JOIN editions e ON e.id = u.edition_id
    WHERE i.caller = ? AND (a.state = 'needs_review' OR j.state IN ('needs_review', 'failed'))"
    };
}
macro_rules! direct_filter {
    () => {
        "FROM direct_acquisitions d
    JOIN units u ON u.id = d.unit_id
    JOIN editions e ON e.id = u.edition_id
    WHERE d.owner = ? AND d.state = 'needs_review'"
    };
}
// Same association predicate as the inventory entries endpoint.
macro_rules! entry_filter {
    () => {
        "FROM scan_entries s
    JOIN library_roots r ON r.id = s.root_id
    LEFT JOIN scan_runs sr ON sr.id = s.last_seen_run_id
    WHERE s.state = 'pending_association' AND NOT EXISTS (
        SELECT 1 FROM library_files f JOIN file_coverage c ON c.library_file_id = f.id
        WHERE f.path = rtrim(r.path, '/') || '/' || s.relative_path
          AND f.signature = s.signature AND f.size_bytes = s.size_bytes)"
    };
}
// Acquisition jobs of this caller already surface as acquisition items.
macro_rules! job_filter {
    () => {
        "FROM jobs j
    WHERE j.state IN ('failed', 'needs_review')
      AND NOT EXISTS (SELECT 1 FROM acquisition_intents i WHERE i.job_id = j.id AND i.caller = ?)"
    };
}
macro_rules! monitor_filter {
    () => {
        "FROM monitors m
    JOIN units u ON u.id = m.unit_id
    JOIN editions e ON e.id = u.edition_id
    WHERE m.owner = ? AND m.deleted_at IS NULL AND m.last_state = 'needs_review'"
    };
}

fn item(kind: &'static str, row: &SqliteRow) -> Result<ReviewItem, sqlx::Error> {
    let optional = |name: &str| -> Result<Option<String>, sqlx::Error> {
        match row.try_get::<Option<String>, _>(name) {
            Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
            other => other,
        }
    };
    Ok(ReviewItem {
        kind,
        id: row.try_get("id")?,
        created_at: row.try_get("created_at")?,
        state: row.try_get("state")?,
        reason: optional("reason")?,
        publication_id: optional("publication_id")?,
        unit_id: optional("unit_id")?,
        acquisition_id: optional("acquisition_id")?,
        job_id: optional("job_id")?,
        job_kind: optional("job_kind")?,
        root_id: optional("root_id")?,
        entry_id: optional("entry_id")?,
        relative_path: optional("relative_path")?,
        monitor_id: optional("monitor_id")?,
    })
}

macro_rules! queries {
    ($columns:literal, $filter:ident, $order:literal) => {
        (
            concat!("SELECT COUNT(*) ", $filter!()),
            concat!(
                "SELECT ",
                $columns,
                " ",
                $filter!(),
                " ORDER BY ",
                $order,
                " LIMIT ?"
            ),
        )
    };
}

async fn source(
    store: &SqliteStore,
    kind: &'static str,
    (count_sql, list_sql): (&'static str, &'static str),
    owner: Option<&str>,
    items: &mut Vec<ReviewItem>,
) -> Result<i64, sqlx::Error> {
    let mut count = sqlx::query_scalar::<_, i64>(count_sql);
    if let Some(owner) = owner {
        count = count.bind(owner);
    }
    let total = count.fetch_one(store.reader()).await?;
    let mut list = sqlx::query(list_sql);
    if let Some(owner) = owner {
        list = list.bind(owner);
    }
    for row in list.bind(PER_KIND).fetch_all(store.reader()).await? {
        items.push(item(kind, &row)?);
    }
    Ok(total)
}

pub async fn review_feed(
    store: &SqliteStore,
    user_id: &str,
    scope: Scope,
) -> Result<ReviewFeed, sqlx::Error> {
    let manage = scope.allows(Scope::Manage);
    let mut items = Vec::new();
    let acquisition_needs_review = source(
        store,
        "acquisition_needs_review",
        queries!(
            "a.id, a.created_at, a.state, COALESCE(a.reason, j.reason) AS reason, e.publication_id, a.unit_id, a.id AS acquisition_id, j.id AS job_id, j.kind AS job_kind",
            acquisition_filter,
            "a.created_at DESC, a.id DESC"
        ),
        Some(user_id),
        &mut items,
    )
    .await?;
    let direct_acquisition_needs_review = if manage {
        Some(
            source(
                store,
                "direct_acquisition_needs_review",
                queries!(
                    "d.id, d.created_at, d.state, d.reason, e.publication_id, d.unit_id, d.id AS acquisition_id",
                    direct_filter,
                    "d.created_at DESC, d.id DESC"
                ),
                Some(user_id),
                &mut items,
            )
            .await?,
        )
    } else {
        None
    };
    let file_to_link = if manage {
        Some(
            source(
                store,
                "file_to_link",
                queries!(
                    "s.id, sr.started_at AS created_at, s.state, s.reason, s.root_id, s.id AS entry_id, s.relative_path",
                    entry_filter,
                    "sr.started_at DESC NULLS LAST, s.id DESC"
                ),
                None,
                &mut items,
            )
            .await?,
        )
    } else {
        None
    };
    let job_failed = source(
        store,
        "job_failed",
        queries!(
            "j.id, j.created_at, j.state, j.reason, j.id AS job_id, j.kind AS job_kind",
            job_filter,
            "j.created_at DESC, j.id DESC"
        ),
        Some(user_id),
        &mut items,
    )
    .await?;
    let monitor_needs_review = source(
        store,
        "monitor_needs_review",
        queries!(
            "m.id, COALESCE(m.last_run_at, m.updated_at) AS created_at, m.last_state AS state, m.reason, e.publication_id, m.unit_id, m.id AS monitor_id",
            monitor_filter,
            "COALESCE(m.last_run_at, m.updated_at) DESC, m.id DESC"
        ),
        Some(user_id),
        &mut items,
    )
    .await?;
    items.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.kind.cmp(right.kind))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(ReviewFeed {
        items,
        totals: ReviewTotals {
            acquisition_needs_review,
            direct_acquisition_needs_review,
            file_to_link,
            job_failed,
            monitor_needs_review,
        },
    })
}

async fn feed(
    State(context): State<ReviewContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let principal = authorize(&context.auth, &headers, &Method::GET, Scope::Read).await?;
    let feed = review_feed(&context.store, &principal.user_id, principal.scope)
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(feed)).into_response())
}
