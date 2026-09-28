use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::{CatalogError, ContentType, DatePrecision, Edition, Publication, Unit, UnitKind};
use crate::store::sqlite::SqliteStore;

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

#[derive(Clone)]
pub struct WantedRepository {
    store: SqliteStore,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WantedMonitoring {
    All,
    Monitored,
    Unmonitored,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WantedAvailabilityFilter {
    All,
    Attention,
    Unverified,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WantedFilters {
    pub q: Option<String>,
    pub kind: Option<ContentType>,
    pub publication_id: Option<String>,
    pub monitoring: Option<WantedMonitoring>,
    pub availability: Option<WantedAvailabilityFilter>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssociationStatus {
    None,
    Associated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WantedAvailability {
    ConfirmedPresent,
    Attention,
    Unverified,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct WantedCounts {
    pub associated: i64,
    pub present: i64,
    pub missing: i64,
    pub changed: i64,
    pub scan_error: i64,
    pub scan_unavailable: i64,
    pub not_scan_managed: i64,
    pub not_observed: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnitContext {
    pub publication: Publication,
    pub edition: Edition,
    pub unit: Unit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WantedUnit {
    pub context: UnitContext,
    pub counts: WantedCounts,
    pub association_status: AssociationStatus,
    pub availability: WantedAvailability,
    pub monitor_count: i64,
    pub enabled_monitor_count: i64,
    /// Same definition as PublicationSummary.cover_file_id.
    pub publication_cover_file_id: Option<String>,
}

/// Unit counts for one publication across every unit matching the request
/// filters, independent of the page window. Categories overlap.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct WantedPublicationTotals {
    pub publication_id: String,
    pub total: i64,
    pub missing: i64,
    pub unverified: i64,
    pub present: i64,
    pub changed: i64,
    pub scan_problem: i64,
    pub monitored: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WantedPage {
    pub items: Vec<WantedUnit>,
    pub next_cursor: Option<String>,
    pub publication_totals: Vec<WantedPublicationTotals>,
}

#[derive(Deserialize, Serialize)]
struct Cursor {
    owner: String,
    filters: WantedFilters,
    sort_title: String,
    publication_id: String,
    edition_id: String,
    unit_sort: String,
    unit_id: String,
}

impl WantedRepository {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn unit_context(&self, id: &str) -> Result<Option<UnitContext>, CatalogError> {
        let row = sqlx::query(CONTEXT_SQL)
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?;
        row.map(|row| context_row(&row)).transpose()
    }

    pub async fn list(
        &self,
        owner: &str,
        filters: WantedFilters,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<WantedPage, CatalogError> {
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid(
                "limit must be between 1 and 100".into(),
            ));
        }
        let filters = normalize_filters(filters)?;
        let mut cursor: Option<Cursor> = cursor.map(decode_cursor).transpose()?;
        if let Some(cursor) = &mut cursor {
            cursor.filters = normalize_filters(cursor.filters.clone())?;
        }
        if cursor
            .as_ref()
            .is_some_and(|value| value.owner != owner || !same_filters(&value.filters, &filters))
        {
            return Err(CatalogError::Invalid("cursor does not match filter".into()));
        }
        let cursor = cursor.as_ref();
        let rows = bind_filters(sqlx::query(LIST_SQL), owner, &filters)
            .bind(cursor.map(|value| value.sort_title.as_str()))
            .bind(cursor.map(|value| value.sort_title.as_str()))
            .bind(cursor.map(|value| value.sort_title.as_str()))
            .bind(cursor.map(|value| value.publication_id.as_str()))
            .bind(cursor.map(|value| value.publication_id.as_str()))
            .bind(cursor.map(|value| value.edition_id.as_str()))
            .bind(cursor.map(|value| value.edition_id.as_str()))
            .bind(cursor.map(|value| value.unit_sort.as_str()))
            .bind(cursor.map(|value| value.unit_sort.as_str()))
            .bind(cursor.map(|value| value.unit_id.as_str()))
            .bind(i64::from(limit) + 1)
            .bind(owner)
            .fetch_all(self.store.reader())
            .await?;
        let mut items = rows
            .into_iter()
            .map(wanted_row)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let mut publication_ids: Vec<&str> = items
            .iter()
            .map(|item| item.context.publication.id.as_str())
            .collect();
        publication_ids.dedup();
        let covers = super::CatalogRepository::new(self.store.clone())
            .cover_file_ids(&publication_ids)
            .await?;
        let publication_totals = self
            .publication_totals(owner, &filters, &publication_ids)
            .await?;
        for item in &mut items {
            item.publication_cover_file_id = covers.get(&item.context.publication.id).cloned();
        }
        let next_cursor = if has_more {
            let item = items.last().expect("nonempty page has final item");
            Some(encode_cursor(&Cursor {
                owner: owner.into(),
                filters: filters.clone(),
                sort_title: item.context.publication.sort_title.clone(),
                publication_id: item.context.publication.id.clone(),
                edition_id: item.context.edition.id.clone(),
                unit_sort: item
                    .context
                    .unit
                    .sort_key
                    .clone()
                    .unwrap_or_else(|| item.context.unit.label.clone()),
                unit_id: item.context.unit.id.clone(),
            }))
        } else {
            None
        };
        Ok(WantedPage {
            items,
            next_cursor,
            publication_totals,
        })
    }

    /// Aggregates only the publications already on the page, so cost is
    /// bounded by the page size rather than the library.
    async fn publication_totals(
        &self,
        owner: &str,
        filters: &WantedFilters,
        publication_ids: &[&str],
    ) -> Result<Vec<WantedPublicationTotals>, CatalogError> {
        if publication_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = serde_json::to_string(publication_ids).expect("ids are serializable");
        let rows = bind_filters(sqlx::query(TOTALS_SQL), owner, filters)
            .bind(ids)
            .bind(owner)
            .fetch_all(self.store.reader())
            .await?;
        let mut totals = rows
            .into_iter()
            .map(|row| {
                Ok(WantedPublicationTotals {
                    publication_id: row.try_get("publication_id")?,
                    total: row.try_get("total")?,
                    missing: row.try_get("missing")?,
                    unverified: row.try_get("unverified")?,
                    present: row.try_get("present")?,
                    changed: row.try_get("changed")?,
                    scan_problem: row.try_get("scan_problem")?,
                    monitored: row.try_get("monitored")?,
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;
        totals.sort_by_key(|value| {
            publication_ids
                .iter()
                .position(|id| *id == value.publication_id)
        });
        Ok(totals)
    }
}

fn bind_filters<'q>(
    query: SqliteQuery<'q>,
    owner: &'q str,
    filters: &'q WantedFilters,
) -> SqliteQuery<'q> {
    let monitoring = filters.monitoring.as_ref();
    let availability = filters.availability.as_ref();
    query
        .bind(filters.q.as_deref())
        .bind(filters.q.as_deref().unwrap_or(""))
        .bind(filters.q.as_deref().unwrap_or(""))
        .bind(filters.q.as_deref().unwrap_or(""))
        .bind(filters.kind.as_ref().map(ContentType::as_str))
        .bind(filters.kind.as_ref().map(ContentType::as_str))
        .bind(filters.publication_id.as_deref())
        .bind(filters.publication_id.as_deref())
        .bind(monitoring.is_some_and(|value| *value == WantedMonitoring::Monitored))
        .bind(owner)
        .bind(monitoring.is_some_and(|value| *value == WantedMonitoring::Unmonitored))
        .bind(owner)
        .bind(availability.is_some_and(|value| *value == WantedAvailabilityFilter::All))
        .bind(availability.is_some_and(|value| *value == WantedAvailabilityFilter::Unverified))
        .bind(availability.is_some_and(|value| *value == WantedAvailabilityFilter::Attention))
}

pub(crate) const CONTEXT_SQL: &str = r#"
SELECT p.id AS p_id, p.content_type AS p_content_type, p.title AS p_title, p.sort_title AS p_sort_title, p.run_label AS p_run_label, p.title_locked AS p_title_locked, p.known_unit_count AS p_known_unit_count,
       e.id AS e_id, e.publication_id AS e_publication_id, e.language AS e_language, e.region AS e_region, e.publisher AS e_publisher,
       u.id AS u_id, u.edition_id AS u_edition_id, u.label AS u_label, u.kind AS u_kind, u.sort_key AS u_sort_key, u.date AS u_date, u.date_precision AS u_date_precision
FROM units u JOIN editions e ON e.id = u.edition_id JOIN publications p ON p.id = e.publication_id
WHERE u.id = ?
"#;

// Shared by LIST_SQL and TOTALS_SQL so page items and totals classify units
// identically. Binds are supplied by bind_filters.
macro_rules! wanted_match {
    () => {
        r#"
    FROM publications p
    CROSS JOIN editions e ON e.publication_id = p.id
    CROSS JOIN units u ON u.edition_id = e.id
    WHERE (? IS NULL OR instr(lower(p.title), ?) > 0 OR instr(lower(COALESCE(p.run_label, '')), ?) > 0 OR instr(lower(u.label), ?) > 0)
      AND (? IS NULL OR p.content_type = ?) AND (? IS NULL OR p.id = ?)
      AND (? = 0 OR EXISTS (
          SELECT 1 FROM monitors m
          WHERE m.owner = ? AND m.unit_id = u.id AND m.deleted_at IS NULL AND m.enabled = 1
      ))
      AND (? = 0 OR NOT EXISTS (
          SELECT 1 FROM monitors m
          WHERE m.owner = ? AND m.unit_id = u.id AND m.deleted_at IS NULL AND m.enabled = 1
      ))
      AND (? = 1
          OR (? = 1
              AND EXISTS (SELECT 1 FROM file_coverage c WHERE c.unit_id = u.id)
              AND NOT EXISTS (
                  SELECT 1
                  FROM file_coverage c
                  JOIN library_files f ON f.id = c.library_file_id
                  LEFT JOIN library_roots r ON r.id = f.root_id
                  LEFT JOIN scan_entries e ON e.root_id = f.root_id AND e.relative_path = f.relative_path
                      AND f.path = rtrim(r.path, '/') || '/' || f.relative_path
                  WHERE c.unit_id = u.id
                    AND e.state = 'pending_association' AND e.signature = f.signature AND e.size_bytes = f.size_bytes
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM file_coverage c
                  JOIN library_files f ON f.id = c.library_file_id
                  LEFT JOIN library_roots r ON r.id = f.root_id
                  LEFT JOIN scan_entries e ON e.root_id = f.root_id AND e.relative_path = f.relative_path
                      AND f.path = rtrim(r.path, '/') || '/' || f.relative_path
                  WHERE c.unit_id = u.id AND f.root_id IS NOT NULL AND f.relative_path IS NOT NULL
                    AND e.id IS NOT NULL
                    AND NOT COALESCE((e.state = 'pending_association' AND e.signature = f.signature AND e.size_bytes = f.size_bytes), 0)
              )
          )
          OR (? = 1 AND (
              NOT EXISTS (SELECT 1 FROM file_coverage c WHERE c.unit_id = u.id)
              OR EXISTS (
                  SELECT 1
                  FROM file_coverage c
                  JOIN library_files f ON f.id = c.library_file_id
                  LEFT JOIN library_roots r ON r.id = f.root_id
                  LEFT JOIN scan_entries e ON e.root_id = f.root_id AND e.relative_path = f.relative_path
                      AND f.path = rtrim(r.path, '/') || '/' || f.relative_path
                  WHERE c.unit_id = u.id AND f.root_id IS NOT NULL AND f.relative_path IS NOT NULL
                    AND e.id IS NOT NULL
                    AND NOT COALESCE((e.state = 'pending_association' AND e.signature = f.signature AND e.size_bytes = f.size_bytes), 0)
              )
          ))
      )
"#
    };
}

// Per-unit file status and caller-owned monitor counts for the `page` CTE.
macro_rules! wanted_unit_counts {
    () => {
        r#"
file_status AS (
    SELECT c.unit_id,
        CASE
            WHEN f.root_id IS NULL OR f.relative_path IS NULL THEN 'not_scan_managed'
            WHEN e.id IS NULL THEN 'not_observed'
            WHEN e.state = 'pending_association' AND e.signature = f.signature AND e.size_bytes = f.size_bytes THEN 'present'
            WHEN e.state = 'pending_association' THEN 'changed'
            WHEN e.state = 'missing' THEN 'missing'
            WHEN e.state = 'error' THEN 'scan_error'
            ELSE 'scan_unavailable'
        END AS status
    FROM page p
    CROSS JOIN file_coverage c ON c.unit_id = p.u_id
    JOIN library_files f ON f.id = c.library_file_id
    LEFT JOIN library_roots r ON r.id = f.root_id
    LEFT JOIN scan_entries e ON e.root_id = f.root_id AND e.relative_path = f.relative_path
        AND f.path = rtrim(r.path, '/') || '/' || f.relative_path
), status_counts AS (
    SELECT unit_id, COUNT(*) AS associated,
        SUM(status = 'present') AS present, SUM(status = 'missing') AS missing,
        SUM(status = 'changed') AS changed, SUM(status = 'scan_error') AS scan_error,
        SUM(status = 'scan_unavailable') AS scan_unavailable,
        SUM(status = 'not_scan_managed') AS not_scan_managed, SUM(status = 'not_observed') AS not_observed
    FROM file_status GROUP BY unit_id
), monitor_counts AS (
    SELECT m.unit_id, COUNT(*) AS monitor_count, SUM(m.enabled = 1) AS enabled_monitor_count
    FROM page p
    CROSS JOIN monitors m ON m.unit_id = p.u_id
    WHERE m.owner = ? AND m.deleted_at IS NULL
    GROUP BY m.unit_id
)
"#
    };
}

const LIST_SQL: &str = concat!(
    r#"
WITH page AS (
    SELECT p.id AS p_id, p.content_type AS p_content_type, p.title AS p_title, p.sort_title AS p_sort_title, p.run_label AS p_run_label, p.title_locked AS p_title_locked, p.known_unit_count AS p_known_unit_count,
           e.id AS e_id, e.publication_id AS e_publication_id, e.language AS e_language, e.region AS e_region, e.publisher AS e_publisher,
           u.id AS u_id, u.edition_id AS u_edition_id, u.label AS u_label, u.kind AS u_kind, u.sort_key AS u_sort_key, u.date AS u_date, u.date_precision AS u_date_precision
"#,
    wanted_match!(),
    r#"
      AND (? IS NULL OR p.sort_title > ? OR (p.sort_title = ? AND (p.id > ? OR (p.id = ? AND (e.id > ? OR (e.id = ? AND (COALESCE(u.sort_key, u.label) > ? OR (COALESCE(u.sort_key, u.label) = ? AND u.id > ?))))))))
    ORDER BY p.sort_title, p.id, e.id, COALESCE(u.sort_key, u.label), u.id
    LIMIT ?
),"#,
    wanted_unit_counts!(),
    r#"
SELECT p.*, COALESCE(s.associated, 0) AS associated, COALESCE(s.present, 0) AS present,
       COALESCE(s.missing, 0) AS missing, COALESCE(s.changed, 0) AS changed,
       COALESCE(s.scan_error, 0) AS scan_error, COALESCE(s.scan_unavailable, 0) AS scan_unavailable,
       COALESCE(s.not_scan_managed, 0) AS not_scan_managed, COALESCE(s.not_observed, 0) AS not_observed,
       COALESCE(m.monitor_count, 0) AS monitor_count, COALESCE(m.enabled_monitor_count, 0) AS enabled_monitor_count
FROM page p
LEFT JOIN status_counts s ON s.unit_id = p.u_id
LEFT JOIN monitor_counts m ON m.unit_id = p.u_id
ORDER BY p_sort_title, p_id, e_id, COALESCE(u_sort_key, u_label), u_id
"#
);

// Unit predicates mirror wanted_row/availability: missing means no
// association or every associated file missing; unverified and present match
// WantedAvailability; monitored means an enabled caller-owned monitor.
const TOTALS_SQL: &str = concat!(
    r#"
WITH page AS (
    SELECT p.id AS p_id, u.id AS u_id
"#,
    wanted_match!(),
    r#"
      AND p.id IN (SELECT value FROM json_each(?))
),"#,
    wanted_unit_counts!(),
    r#"
SELECT p.p_id AS publication_id, COUNT(*) AS total,
       SUM(COALESCE(s.associated, 0) = 0 OR s.missing = s.associated) AS missing,
       SUM(COALESCE(s.associated, 0) > 0 AND COALESCE(s.present, 0) = 0
           AND COALESCE(s.missing + s.changed + s.scan_error + s.scan_unavailable, 0) = 0) AS unverified,
       SUM(COALESCE(s.present, 0) > 0) AS present,
       SUM(COALESCE(s.changed, 0) > 0) AS changed,
       SUM(COALESCE(s.scan_error + s.scan_unavailable, 0) > 0) AS scan_problem,
       SUM(COALESCE(m.enabled_monitor_count, 0) > 0) AS monitored
FROM page p
LEFT JOIN status_counts s ON s.unit_id = p.u_id
LEFT JOIN monitor_counts m ON m.unit_id = p.u_id
GROUP BY p.p_id
"#
);

fn normalize_filters(mut filters: WantedFilters) -> Result<WantedFilters, CatalogError> {
    if let Some(q) = &filters.q {
        if q.len() > 256 || q.chars().any(char::is_control) {
            return Err(CatalogError::Invalid(
                "q must be at most 256 UTF-8 bytes and contain no control characters".into(),
            ));
        }
        filters.q = (!q.trim().is_empty()).then(|| q.trim().to_ascii_lowercase());
    }
    filters.monitoring.get_or_insert(WantedMonitoring::All);
    filters
        .availability
        .get_or_insert(WantedAvailabilityFilter::Attention);
    Ok(filters)
}
fn same_filters(a: &WantedFilters, b: &WantedFilters) -> bool {
    a.q == b.q
        && a.kind == b.kind
        && a.publication_id == b.publication_id
        && a.monitoring == b.monitoring
        && a.availability == b.availability
}
fn encode_cursor(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("cursor is serializable"))
}
fn decode_cursor(value: &str) -> Result<Cursor, CatalogError> {
    if value.len() > 8 * 1024 {
        return Err(CatalogError::Invalid("cursor is too long".into()));
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| CatalogError::Invalid("invalid cursor".into()))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|_| CatalogError::Invalid("invalid cursor".into()))
        })
}
pub(crate) fn context_row(row: &sqlx::sqlite::SqliteRow) -> Result<UnitContext, CatalogError> {
    Ok(UnitContext {
        publication: Publication {
            id: row.try_get("p_id")?,
            content_type: ContentType::parse(row.try_get("p_content_type")?)
                .map_err(CatalogError::Invalid)?,
            title: row.try_get("p_title")?,
            sort_title: row.try_get("p_sort_title")?,
            run_label: row.try_get("p_run_label")?,
            title_locked: row.try_get::<i64, _>("p_title_locked")? != 0,
            known_unit_count: row.try_get("p_known_unit_count")?,
        },
        edition: Edition {
            id: row.try_get("e_id")?,
            publication_id: row.try_get("e_publication_id")?,
            language: row.try_get("e_language")?,
            region: row.try_get("e_region")?,
            publisher: row.try_get("e_publisher")?,
        },
        unit: Unit {
            id: row.try_get("u_id")?,
            edition_id: row.try_get("u_edition_id")?,
            label: row.try_get("u_label")?,
            kind: UnitKind::parse(row.try_get("u_kind")?).map_err(CatalogError::Invalid)?,
            sort_key: row.try_get("u_sort_key")?,
            date: row.try_get("u_date")?,
            date_precision: row
                .try_get::<Option<String>, _>("u_date_precision")?
                .map(DatePrecision::parse)
                .transpose()
                .map_err(CatalogError::Invalid)?,
        },
    })
}
fn wanted_row(row: sqlx::sqlite::SqliteRow) -> Result<WantedUnit, CatalogError> {
    let counts = WantedCounts {
        associated: row.try_get("associated")?,
        present: row.try_get("present")?,
        missing: row.try_get("missing")?,
        changed: row.try_get("changed")?,
        scan_error: row.try_get("scan_error")?,
        scan_unavailable: row.try_get("scan_unavailable")?,
        not_scan_managed: row.try_get("not_scan_managed")?,
        not_observed: row.try_get("not_observed")?,
    };
    let association_status = if counts.associated == 0 {
        AssociationStatus::None
    } else {
        AssociationStatus::Associated
    };
    Ok(WantedUnit {
        context: context_row(&row)?,
        counts: counts.clone(),
        association_status,
        availability: availability(&counts),
        monitor_count: row.try_get("monitor_count")?,
        enabled_monitor_count: row.try_get("enabled_monitor_count")?,
        publication_cover_file_id: None,
    })
}
fn availability(counts: &WantedCounts) -> WantedAvailability {
    if counts.present > 0 {
        WantedAvailability::ConfirmedPresent
    } else if counts.missing + counts.changed + counts.scan_error + counts.scan_unavailable > 0 {
        WantedAvailability::Attention
    } else {
        WantedAvailability::Unverified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, SqliteStore, WantedRepository) {
        let directory = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let store = SqliteStore::open(directory.path()).await.unwrap();
        let mut tx = store.begin_write().await.unwrap();
        for statement in [
            "INSERT INTO publications(id,content_type,title,sort_title,known_unit_count) VALUES('p','comic','100%_\\\\ title','a',99)",
            "INSERT INTO editions(id,publication_id,language) VALUES('e','p','en')",
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u','e','100%_\\\\ unit','issue','1')",
            "INSERT INTO library_roots(id,label,path) VALUES('r','Root','/library')",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        (directory, store.clone(), WantedRepository::new(store))
    }

    async fn file(
        store: &SqliteStore,
        id: &str,
        root: Option<&str>,
        relative: Option<&str>,
        signature: &str,
        size: i64,
    ) {
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path) VALUES(?,?,'cbz',?,?,?,?)")
            .bind(id).bind(format!("/library/{id}.cbz")).bind(signature).bind(size).bind(root).bind(relative)
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES(?,'u','user_confirmed')").bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn lists_explicit_units_with_literal_queries_owner_monitors_and_cursors() {
        let (_directory, store, repository) = fixture().await;
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u2','e','100%_\\\\ second','issue','2')").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES('i','comicvine','Source','https://source',1,'{}',1,zeroblob(24),zeroblob(28))").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m','alice','u','i','comic','q',900,'review_only',1,0,'scheduled')").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let filters = WantedFilters {
            q: Some("100%_\\\\".into()),
            availability: Some(WantedAvailabilityFilter::All),
            ..WantedFilters::default()
        };
        let page = repository
            .list("alice", filters.clone(), 1, None)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].monitor_count, 1);
        assert!(page.next_cursor.is_some());
        let second = repository
            .list("alice", filters.clone(), 1, page.next_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next_cursor.is_none());
        assert_eq!(
            repository
                .list("bob", filters.clone(), 1, page.next_cursor.as_deref())
                .await
                .unwrap_err()
                .to_string(),
            "invalid catalog value: cursor does not match filter"
        );
        assert!(
            repository
                .list(
                    "alice",
                    WantedFilters {
                        q: Some("x".repeat(257)),
                        ..filters
                    },
                    1,
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(
            repository
                .list(
                    "alice",
                    WantedFilters {
                        monitoring: Some(WantedMonitoring::Unmonitored),
                        availability: Some(WantedAvailabilityFilter::All),
                        ..WantedFilters::default()
                    },
                    100,
                    None
                )
                .await
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn items_carry_their_publication_cover_in_edition_then_unit_order() {
        let (_directory, store, repository) = fixture().await;
        let mut tx = store.begin_write().await.unwrap();
        for statement in [
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u0','e','first','issue','0')",
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u2','e','uncovered','issue','2')",
            "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p2','comic','Empty','b')",
            "INSERT INTO editions(id,publication_id,language) VALUES('e2','p2','en')",
            "INSERT INTO units(id,edition_id,label,kind) VALUES('v','e2','1','issue')",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        file(&store, "a-later-unit", None, None, "sig", 1).await;
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('z-first-unit','/library/z.cbz','cbz','sig',1)").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('z-first-unit','u0','user_confirmed')").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let filters = WantedFilters {
            availability: Some(WantedAvailabilityFilter::All),
            ..WantedFilters::default()
        };
        let mut covers = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = repository
                .list("owner", filters.clone(), 1, cursor.as_deref())
                .await
                .unwrap();
            covers.extend(
                page.items
                    .into_iter()
                    .map(|item| (item.context.unit.id, item.publication_cover_file_id)),
            );
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let cover = Some("z-first-unit".to_owned());
        assert_eq!(
            covers,
            vec![
                ("u0".to_owned(), cover.clone()),
                ("u".to_owned(), cover.clone()),
                ("u2".to_owned(), cover.clone()),
                ("v".to_owned(), None),
            ]
        );
        let summary = crate::catalog::CatalogRepository::new(store.clone())
            .get_publication_summary("p", "owner")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.cover_file_id, cover);
    }

    #[tokio::test]
    async fn reports_present_faults_changed_missing_and_unobserved_files() {
        let (_directory, store, repository) = fixture().await;
        file(&store, "present", Some("r"), Some("present.cbz"), "same", 1).await;
        file(&store, "changed", Some("r"), Some("changed.cbz"), "old", 1).await;
        file(&store, "missing", Some("r"), Some("missing.cbz"), "same", 1).await;
        file(&store, "error", Some("r"), Some("error.cbz"), "same", 1).await;
        file(&store, "skipped", Some("r"), Some("skipped.cbz"), "same", 1).await;
        file(
            &store,
            "unobserved",
            Some("r"),
            Some("unobserved.cbz"),
            "same",
            1,
        )
        .await;
        let mut tx = store.begin_write().await.unwrap();
        for statement in [
            "INSERT INTO scan_entries(id,root_id,relative_path,signature,size_bytes,mtime_ns,state) VALUES('se1','r','present.cbz','same',1,1,'pending_association')",
            "INSERT INTO scan_entries(id,root_id,relative_path,signature,size_bytes,mtime_ns,state) VALUES('se2','r','changed.cbz','new',1,1,'pending_association')",
            "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('se3','r','missing.cbz','missing')",
            "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('se4','r','error.cbz','error')",
            "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('se5','r','skipped.cbz','skipped')",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        let mut page = repository
            .list(
                "owner",
                WantedFilters {
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        let item = page.items.remove(0);
        assert_eq!(item.availability, WantedAvailability::ConfirmedPresent);
        // One usable file must not hide faults in other files covering the same unit.
        let attention = repository
            .list("owner", WantedFilters::default(), 10, None)
            .await
            .unwrap();
        assert_eq!(attention.items.len(), 1);
        assert_eq!(
            (
                item.counts.present,
                item.counts.changed,
                item.counts.missing,
                item.counts.not_observed,
                item.counts.scan_error,
                item.counts.scan_unavailable
            ),
            (1, 1, 1, 1, 1, 1)
        );
    }

    #[tokio::test]
    async fn classifies_non_root_coverage_as_unverified() {
        let (_directory, store, repository) = fixture().await;
        file(&store, "legacy", None, None, "same", 1).await;
        let mut page = repository
            .list(
                "owner",
                WantedFilters {
                    availability: Some(WantedAvailabilityFilter::Unverified),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        let item = page.items.remove(0);
        assert_eq!(item.counts.not_scan_managed, 1);
        assert_eq!(item.availability, WantedAvailability::Unverified);
        assert_eq!(
            repository.unit_context("u").await.unwrap().unwrap().unit.id,
            "u"
        );
    }

    async fn totals_fixture() -> (tempfile::TempDir, SqliteStore, WantedRepository) {
        let (directory, store, repository) = fixture().await;
        let mut tx = store.begin_write().await.unwrap();
        for statement in [
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u2','e','second','issue','2')",
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u3','e','third','issue','3')",
            "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('u4','e','fourth','issue','4')",
            "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p2','manga','Other','b')",
            "INSERT INTO editions(id,publication_id,language) VALUES('e2','p2','en')",
            "INSERT INTO units(id,edition_id,label,kind) VALUES('v','e2','1','chapter')",
            "INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES('i','comicvine','Source','https://source',1,'{}',1,zeroblob(24),zeroblob(28))",
            "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m1','alice','u','i','comic','q',900,'review_only',1,0,'scheduled')",
            "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m2','bob','u2','i','comic','q',900,'review_only',1,0,'scheduled')",
            "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m3','alice','u3','i','comic','q',900,'review_only',0,0,'scheduled')",
            "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('s1','r','fm.cbz','missing')",
            "INSERT INTO scan_entries(id,root_id,relative_path,signature,size_bytes,mtime_ns,state) VALUES('s2','r','fp.cbz','same',1,1,'pending_association')",
            "INSERT INTO scan_entries(id,root_id,relative_path,signature,size_bytes,mtime_ns,state) VALUES('s3','r','fc.cbz','new',1,1,'pending_association')",
            "INSERT INTO scan_entries(id,root_id,relative_path,state) VALUES('s4','r','fe.cbz','error')",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        for (id, unit) in [("fm", "u"), ("fp", "u3"), ("fc", "u4"), ("fe", "u4")] {
            file(&store, id, Some("r"), Some(&format!("{id}.cbz")), "same", 1).await;
            if unit != "u" {
                let mut tx = store.begin_write().await.unwrap();
                sqlx::query("UPDATE file_coverage SET unit_id = ? WHERE library_file_id = ?")
                    .bind(unit)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        }
        (directory, store, repository)
    }

    fn totals(
        publication_id: &str,
        [
            total,
            missing,
            unverified,
            present,
            changed,
            scan_problem,
            monitored,
        ]: [i64; 7],
    ) -> WantedPublicationTotals {
        WantedPublicationTotals {
            publication_id: publication_id.into(),
            total,
            missing,
            unverified,
            present,
            changed,
            scan_problem,
            monitored,
        }
    }

    async fn all_pages(
        repository: &WantedRepository,
        owner: &str,
        filters: WantedFilters,
        limit: u32,
    ) -> Vec<WantedPage> {
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = repository
                .list(owner, filters.clone(), limit, cursor.as_deref())
                .await
                .unwrap();
            cursor = page.next_cursor.clone();
            pages.push(page);
            if cursor.is_none() {
                return pages;
            }
        }
    }

    #[tokio::test]
    async fn publication_totals_cover_all_matching_units_regardless_of_page() {
        let (_directory, _store, repository) = totals_fixture().await;
        let all = WantedFilters {
            availability: Some(WantedAvailabilityFilter::All),
            ..WantedFilters::default()
        };
        let expected_p = totals("p", [4, 2, 0, 1, 1, 1, 1]);
        let expected_p2 = totals("p2", [1, 1, 0, 0, 0, 0, 0]);
        let pages = all_pages(&repository, "alice", all.clone(), 1).await;
        assert_eq!(pages.len(), 5);
        for page in &pages {
            let expected = if page.items[0].context.publication.id == "p" {
                &expected_p
            } else {
                &expected_p2
            };
            assert_eq!(page.publication_totals, vec![expected.clone()]);
        }
        let whole = repository
            .list("alice", all.clone(), 100, None)
            .await
            .unwrap();
        assert_eq!(
            whole.publication_totals,
            vec![expected_p.clone(), expected_p2]
        );
        let item_total: usize = whole.items.len();
        assert_eq!(
            whole
                .publication_totals
                .iter()
                .map(|value| value.total as usize)
                .sum::<usize>(),
            item_total
        );
    }

    #[tokio::test]
    async fn publication_totals_respect_filters_and_owner_monitors() {
        let (_directory, _store, repository) = totals_fixture().await;
        let attention = repository
            .list("alice", WantedFilters::default(), 1, None)
            .await
            .unwrap();
        assert_eq!(
            attention.publication_totals,
            vec![totals("p", [3, 2, 0, 0, 1, 1, 1])]
        );
        let searched = repository
            .list(
                "alice",
                WantedFilters {
                    q: Some("SECOND".into()),
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            searched.publication_totals,
            vec![totals("p", [1, 1, 0, 0, 0, 0, 0])]
        );
        let monitored = repository
            .list(
                "alice",
                WantedFilters {
                    monitoring: Some(WantedMonitoring::Monitored),
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            monitored.publication_totals,
            vec![totals("p", [1, 1, 0, 0, 0, 0, 1])]
        );
        let kind = repository
            .list(
                "alice",
                WantedFilters {
                    kind: Some(ContentType::Manga),
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            kind.publication_totals,
            vec![totals("p2", [1, 1, 0, 0, 0, 0, 0])]
        );
        let bob = repository
            .list(
                "bob",
                WantedFilters {
                    publication_id: Some("p".into()),
                    monitoring: Some(WantedMonitoring::Monitored),
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert_eq!(bob.items[0].context.unit.id, "u2");
        assert_eq!(
            bob.publication_totals,
            vec![totals("p", [1, 1, 0, 0, 0, 0, 1])]
        );
        let carol = repository
            .list(
                "carol",
                WantedFilters {
                    availability: Some(WantedAvailabilityFilter::All),
                    ..WantedFilters::default()
                },
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(carol.publication_totals[0].monitored, 0);
        let empty = repository
            .list(
                "alice",
                WantedFilters {
                    q: Some("nothing matches".into()),
                    ..WantedFilters::default()
                },
                10,
                None,
            )
            .await
            .unwrap();
        assert!(empty.items.is_empty() && empty.publication_totals.is_empty());
    }
}
