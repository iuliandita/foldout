use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use super::{
    Availability, ContentType, DatePrecision, Edition, EditionUpdate, NewEdition, NewProviderLink,
    NewPublication, NewUnit, Page, ProviderLink, Publication, PublicationFilter, PublicationSort,
    PublicationSummary, PublicationUpdate, Unit, UnitKind, UnitUpdate, identity,
};
use crate::store::sqlite::SqliteStore;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("invalid catalog value: {0}")]
    Invalid(String),
    #[error("catalog record conflicts with existing data")]
    Conflict,
    #[error("catalog record was not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct CatalogRepository {
    store: SqliteStore,
}

/// Legacy cursors carry only `q`, `sort_title`, `id`, and `kind`; they decode as title order.
#[derive(Deserialize, Serialize)]
struct Cursor {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    sort: PublicationSort,
    #[serde(default)]
    availability: Availability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    id: String,
    kind: Option<ContentType>,
}

#[derive(Deserialize, Serialize)]
struct EditionCursor {
    #[serde(default)]
    q: Option<String>,
    publication_id: String,
    language: String,
    region: String,
    id: String,
}

#[derive(Deserialize, Serialize)]
struct UnitCursor {
    #[serde(default)]
    q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<UnitKind>,
    edition_id: String,
    sort: String,
    id: String,
}

impl CatalogRepository {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn create_publication(
        &self,
        input: NewPublication,
    ) -> Result<Publication, CatalogError> {
        identity::required(&input.title, "title")?;
        if input.known_unit_count.is_some_and(|count| count < 0) {
            return Err(CatalogError::Invalid(
                "known_unit_count cannot be negative".into(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let sort_title = input.sort_title.unwrap_or_else(|| input.title.clone());
        identity::required(&sort_title, "sort_title")?;
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("INSERT INTO publications (id, content_type, title, sort_title, run_label, title_locked, known_unit_count, created_at) VALUES (?, ?, ?, ?, ?, 0, ?, unixepoch())")
            .bind(&id).bind(input.content_type.as_str()).bind(&input.title).bind(&sort_title).bind(&input.run_label).bind(input.known_unit_count).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(Publication {
            id,
            content_type: input.content_type,
            title: input.title,
            sort_title,
            run_label: input.run_label,
            title_locked: false,
            known_unit_count: input.known_unit_count,
        })
    }

    pub async fn get_publication(&self, id: &str) -> Result<Option<Publication>, CatalogError> {
        let row = sqlx::query("SELECT id, content_type, title, sort_title, run_label, title_locked, known_unit_count FROM publications WHERE id = ?").bind(id).fetch_optional(self.store.reader()).await?;
        row.map(publication_row).transpose()
    }

    pub async fn list_publications(
        &self,
        limit: u32,
        cursor: Option<&str>,
        kind: Option<ContentType>,
    ) -> Result<Page<Publication>, CatalogError> {
        self.list_publications_search(limit, cursor, kind, None)
            .await
    }

    pub async fn list_publications_search(
        &self,
        limit: u32,
        cursor: Option<&str>,
        kind: Option<ContentType>,
        q: Option<&str>,
    ) -> Result<Page<Publication>, CatalogError> {
        self.list_publications_filtered(
            limit,
            cursor,
            PublicationFilter {
                kind,
                q: q.map(str::to_owned),
                ..PublicationFilter::default()
            },
        )
        .await
    }

    pub async fn list_publications_filtered(
        &self,
        limit: u32,
        cursor: Option<&str>,
        filter: PublicationFilter,
    ) -> Result<Page<Publication>, CatalogError> {
        let q = normalize_search(filter.q.as_deref())?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid(
                "limit must be between 1 and 100".into(),
            ));
        }
        let cursor: Option<Cursor> = cursor.map(decode_cursor).transpose()?;
        if let Some(cursor) = &cursor {
            if cursor.kind != filter.kind
                || cursor.q != q
                || cursor.availability != filter.availability
            {
                return Err(CatalogError::Invalid("cursor does not match filter".into()));
            }
            if cursor.sort != filter.sort {
                return Err(CatalogError::Invalid("cursor does not match sort".into()));
            }
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT p.id, p.content_type, p.title, p.sort_title, p.run_label, p.title_locked, p.known_unit_count, p.created_at FROM publications p WHERE 1 = 1",
        );
        if let Some(q) = &q {
            query
                .push(" AND (instr(lower(p.title), lower(")
                .push_bind(q.clone())
                .push(")) > 0 OR instr(lower(COALESCE(p.run_label, '')), lower(")
                .push_bind(q.clone())
                .push(")) > 0)");
        }
        if let Some(kind) = &filter.kind {
            query
                .push(" AND p.content_type = ")
                .push_bind(kind.as_str());
        }
        match filter.availability {
            Availability::All => {}
            Availability::HasFiles => {
                query.push(" AND EXISTS (").push(HAS_FILES_SQL).push(")");
            }
            Availability::NoFiles => {
                query
                    .push(" AND NOT EXISTS (")
                    .push(HAS_FILES_SQL)
                    .push(")");
            }
        }
        let invalid_cursor = || CatalogError::Invalid("invalid cursor".into());
        match (filter.sort, &cursor) {
            (PublicationSort::Title, Some(cursor)) => {
                let sort_title = cursor.sort_title.clone().ok_or_else(invalid_cursor)?;
                query
                    .push(" AND (p.sort_title > ")
                    .push_bind(sort_title.clone())
                    .push(" OR (p.sort_title = ")
                    .push_bind(sort_title)
                    .push(" AND p.id > ")
                    .push_bind(cursor.id.clone())
                    .push("))");
            }
            (PublicationSort::RecentlyAdded, Some(cursor)) => {
                let created_at = cursor.created_at.ok_or_else(invalid_cursor)?;
                query
                    .push(" AND (p.created_at < ")
                    .push_bind(created_at)
                    .push(" OR (p.created_at = ")
                    .push_bind(created_at)
                    .push(" AND p.id < ")
                    .push_bind(cursor.id.clone())
                    .push("))");
            }
            (_, None) => {}
        }
        query.push(match filter.sort {
            PublicationSort::Title => " ORDER BY p.sort_title, p.id",
            PublicationSort::RecentlyAdded => " ORDER BY p.created_at DESC, p.id DESC",
        });
        query.push(" LIMIT ").push_bind(i64::from(limit) + 1);
        let rows = query.build().fetch_all(self.store.reader()).await?;
        let mut items = Vec::with_capacity(rows.len());
        let mut created = Vec::with_capacity(rows.len());
        for row in rows {
            created.push(row.try_get::<i64, _>("created_at")?);
            items.push(publication_row(row)?);
        }
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            let last = items.last().expect("a nonempty page has a final item");
            let (sort_title, created_at) = match filter.sort {
                PublicationSort::Title => (Some(last.sort_title.clone()), None),
                PublicationSort::RecentlyAdded => (None, Some(created[items.len() - 1])),
            };
            Some(encode_cursor(&Cursor {
                q,
                sort: filter.sort,
                availability: filter.availability,
                sort_title,
                created_at,
                id: last.id.clone(),
                kind: filter.kind,
            }))
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn get_publication_summary(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<Option<PublicationSummary>, CatalogError> {
        let Some(publication) = self.get_publication(id).await? else {
            return Ok(None);
        };
        Ok(self
            .summarize(owner, vec![publication])
            .await?
            .into_iter()
            .next())
    }

    pub async fn list_publication_summaries(
        &self,
        owner: &str,
        limit: u32,
        cursor: Option<&str>,
        filter: PublicationFilter,
    ) -> Result<Page<PublicationSummary>, CatalogError> {
        let page = self
            .list_publications_filtered(limit, cursor, filter)
            .await?;
        Ok(Page {
            items: self.summarize(owner, page.items).await?,
            next_cursor: page.next_cursor,
        })
    }

    async fn summarize(
        &self,
        owner: &str,
        publications: Vec<Publication>,
    ) -> Result<Vec<PublicationSummary>, CatalogError> {
        if publications.is_empty() {
            return Ok(Vec::new());
        }
        let ids = serde_json::to_string(
            &publications
                .iter()
                .map(|publication| publication.id.as_str())
                .collect::<Vec<_>>(),
        )
        .expect("identifiers are serializable");
        let rows = sqlx::query(AVAILABILITY_SQL)
            .bind(&ids)
            .bind(owner)
            .fetch_all(self.store.reader())
            .await?;
        let mut availability = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            availability.insert(
                row.try_get::<String, _>("id")?,
                (
                    row.try_get::<i64, _>("file_count")?,
                    row.try_get::<i64, _>("monitored_unit_count")?,
                    row.try_get::<Option<String>, _>("cover_file_id")?,
                ),
            );
        }
        Ok(publications
            .into_iter()
            .map(|publication| {
                let (file_count, monitored_unit_count, cover_file_id) =
                    availability.remove(&publication.id).unwrap_or((0, 0, None));
                PublicationSummary {
                    publication,
                    file_count,
                    monitored_unit_count,
                    cover_file_id,
                }
            })
            .collect())
    }

    /// Cover file per publication, for the given (page-bounded) publication ids only.
    pub(crate) async fn cover_file_ids(
        &self,
        publication_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, String>, CatalogError> {
        if publication_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let ids = serde_json::to_string(publication_ids).expect("identifiers are serializable");
        let rows = sqlx::query(COVER_SQL)
            .bind(ids)
            .fetch_all(self.store.reader())
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("publication_id")?,
                    row.try_get("library_file_id")?,
                ))
            })
            .collect()
    }

    pub async fn update_publication(
        &self,
        id: &str,
        input: PublicationUpdate,
    ) -> Result<Publication, CatalogError> {
        if input
            .title
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
            || input
                .sort_title
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || input
                .known_unit_count
                .flatten()
                .is_some_and(|value| value < 0)
        {
            return Err(CatalogError::Invalid(
                "publication update contains an invalid value".into(),
            ));
        }
        let mut transaction = self.store.begin_write().await?;
        let current = publication_in_transaction(&mut transaction, id)
            .await?
            .ok_or(CatalogError::NotFound)?;
        let title_was_explicit = input.title.is_some();
        let sort_title_was_explicit = input.sort_title.is_some();
        let title = input.title.unwrap_or_else(|| current.title.clone());
        let sort_title = input
            .sort_title
            .unwrap_or_else(|| current.sort_title.clone());
        let run_label = input.run_label.unwrap_or(current.run_label.clone());
        let known_unit_count = input.known_unit_count.unwrap_or(current.known_unit_count);
        let title_locked = current.title_locked || title_was_explicit || sort_title_was_explicit;
        sqlx::query("UPDATE publications SET title = ?, sort_title = ?, run_label = ?, title_locked = ?, known_unit_count = ? WHERE id = ?").bind(&title).bind(&sort_title).bind(&run_label).bind(title_locked).bind(known_unit_count).bind(id).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(Publication {
            id: id.into(),
            content_type: current.content_type,
            title,
            sort_title,
            run_label,
            title_locked,
            known_unit_count,
        })
    }

    pub async fn refresh_title(&self, id: &str, title: &str) -> Result<Publication, CatalogError> {
        identity::required(title, "title")?;
        let mut transaction = self.store.begin_write().await?;
        let current = publication_in_transaction(&mut transaction, id)
            .await?
            .ok_or(CatalogError::NotFound)?;
        let changed = sqlx::query(
            "UPDATE publications SET title = ?, sort_title = ? WHERE id = ? AND title_locked = 0",
        )
        .bind(title)
        .bind(title)
        .bind(id)
        .execute(&mut *transaction)
        .await?;
        if changed.rows_affected() == 0 {
            let current = publication_in_transaction(&mut transaction, id)
                .await?
                .ok_or(CatalogError::NotFound)?;
            transaction.commit().await?;
            return Ok(current);
        }
        transaction.commit().await?;
        Ok(Publication {
            title: title.into(),
            sort_title: title.into(),
            ..current
        })
    }

    pub async fn delete_publication(&self, id: &str) -> Result<(), CatalogError> {
        let mut transaction = self.store.begin_write().await?;
        let result = sqlx::query("DELETE FROM publications WHERE id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await
            .map_err(map_database)?;
        transaction.commit().await?;
        if result.rows_affected() == 0 {
            return Err(CatalogError::NotFound);
        }
        Ok(())
    }

    pub async fn create_edition(&self, input: NewEdition) -> Result<Edition, CatalogError> {
        identity::required(&input.publication_id, "publication_id")?;
        identity::required(&input.language, "language")?;
        let id = Uuid::new_v4().to_string();
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("INSERT INTO editions (id, publication_id, language, region, publisher) VALUES (?, ?, ?, ?, ?)").bind(&id).bind(&input.publication_id).bind(&input.language).bind(&input.region).bind(&input.publisher).execute(&mut *transaction).await.map_err(map_database)?;
        transaction.commit().await?;
        Ok(Edition {
            id,
            publication_id: input.publication_id,
            language: input.language,
            region: input.region,
            publisher: input.publisher,
        })
    }

    pub async fn update_edition(
        &self,
        id: &str,
        input: EditionUpdate,
    ) -> Result<Edition, CatalogError> {
        if input
            .language
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(CatalogError::Invalid(
                "edition update contains an invalid value".into(),
            ));
        }
        let mut transaction = self.store.begin_write().await?;
        let current = edition_in_transaction(&mut transaction, id)
            .await?
            .ok_or(CatalogError::NotFound)?;
        let language = input.language.unwrap_or_else(|| current.language.clone());
        let region = input.region.unwrap_or(current.region.clone());
        let publisher = input.publisher.unwrap_or(current.publisher.clone());
        sqlx::query("UPDATE editions SET language = ?, region = ?, publisher = ? WHERE id = ?")
            .bind(&language)
            .bind(&region)
            .bind(&publisher)
            .bind(id)
            .execute(&mut *transaction)
            .await
            .map_err(map_database)?;
        transaction.commit().await?;
        Ok(Edition {
            id: id.into(),
            publication_id: current.publication_id,
            language,
            region,
            publisher,
        })
    }

    pub async fn list_editions(&self, publication_id: &str) -> Result<Vec<Edition>, CatalogError> {
        sqlx::query("SELECT id, publication_id, language, region, publisher FROM editions WHERE publication_id = ? ORDER BY language, COALESCE(region, ''), id").bind(publication_id).fetch_all(self.store.reader()).await?.into_iter().map(edition_row).collect()
    }

    pub async fn list_editions_page(
        &self,
        publication_id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Edition>, CatalogError> {
        self.list_editions_page_search(publication_id, limit, cursor, None)
            .await
    }

    pub async fn list_editions_page_search(
        &self,
        publication_id: &str,
        limit: u32,
        cursor: Option<&str>,
        q: Option<&str>,
    ) -> Result<Page<Edition>, CatalogError> {
        let q = normalize_search(q)?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid(
                "limit must be between 1 and 100".into(),
            ));
        }
        let cursor: Option<EditionCursor> = cursor.map(decode_cursor).transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|value| value.publication_id != publication_id || value.q != q)
        {
            return Err(CatalogError::Invalid(
                "cursor does not match publication".into(),
            ));
        }
        let rows = match &cursor {
            Some(cursor) => sqlx::query("SELECT id, publication_id, language, region, publisher FROM editions WHERE (? IS NULL OR instr(lower(language), lower(?)) > 0 OR instr(lower(COALESCE(region, '')), lower(?)) > 0 OR instr(lower(COALESCE(publisher, '')), lower(?)) > 0) AND (publication_id = ? AND (language > ? OR (language = ? AND (COALESCE(region, '') > ? OR (COALESCE(region, '') = ? AND id > ?))))) ORDER BY language, COALESCE(region, ''), id LIMIT ?").bind(q.as_deref()).bind(q.as_deref().unwrap_or("")).bind(q.as_deref().unwrap_or("")).bind(q.as_deref().unwrap_or("")).bind(publication_id).bind(&cursor.language).bind(&cursor.language).bind(&cursor.region).bind(&cursor.region).bind(&cursor.id).bind(i64::from(limit) + 1).fetch_all(self.store.reader()).await?,
            None => sqlx::query("SELECT id, publication_id, language, region, publisher FROM editions WHERE (? IS NULL OR instr(lower(language), lower(?)) > 0 OR instr(lower(COALESCE(region, '')), lower(?)) > 0 OR instr(lower(COALESCE(publisher, '')), lower(?)) > 0) AND (publication_id = ?) ORDER BY language, COALESCE(region, ''), id LIMIT ?").bind(q.as_deref()).bind(q.as_deref().unwrap_or("")).bind(q.as_deref().unwrap_or("")).bind(q.as_deref().unwrap_or("")).bind(publication_id).bind(i64::from(limit) + 1).fetch_all(self.store.reader()).await?,
        };
        let mut items: Vec<_> = rows
            .into_iter()
            .map(edition_row)
            .collect::<Result<_, _>>()?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            let last = items.last().expect("a nonempty page has a final item");
            Some(encode_cursor(&EditionCursor {
                q,
                publication_id: publication_id.into(),
                language: last.language.clone(),
                region: last.region.clone().unwrap_or_default(),
                id: last.id.clone(),
            }))
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn create_unit(&self, input: NewUnit) -> Result<Unit, CatalogError> {
        identity::required(&input.edition_id, "edition_id")?;
        identity::required(&input.label, "label")?;
        let date_precision = input.date.as_deref().map(identity::date).transpose()?;
        let id = Uuid::new_v4().to_string();
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("INSERT INTO units (id, edition_id, label, kind, sort_key, date, date_precision) VALUES (?, ?, ?, ?, ?, ?, ?)").bind(&id).bind(&input.edition_id).bind(&input.label).bind(input.kind.as_str()).bind(&input.sort_key).bind(&input.date).bind(date_precision.as_ref().map(DatePrecision::as_str)).execute(&mut *transaction).await.map_err(map_database)?;
        transaction.commit().await?;
        Ok(Unit {
            id,
            edition_id: input.edition_id,
            label: input.label,
            kind: input.kind,
            sort_key: input.sort_key,
            date: input.date,
            date_precision,
        })
    }

    pub async fn update_unit(&self, id: &str, input: UnitUpdate) -> Result<Unit, CatalogError> {
        if input
            .label
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(CatalogError::Invalid(
                "unit update contains an invalid value".into(),
            ));
        }
        let mut transaction = self.store.begin_write().await?;
        let current = unit_in_transaction(&mut transaction, id)
            .await?
            .ok_or(CatalogError::NotFound)?;
        let label = input.label.unwrap_or_else(|| current.label.clone());
        let kind = input.kind.unwrap_or_else(|| current.kind.clone());
        let sort_key = input.sort_key.unwrap_or(current.sort_key.clone());
        let date = input.date.unwrap_or(current.date.clone());
        let date_precision = date.as_deref().map(identity::date).transpose()?;
        sqlx::query("UPDATE units SET label = ?, kind = ?, sort_key = ?, date = ?, date_precision = ? WHERE id = ?")
            .bind(&label).bind(kind.as_str()).bind(&sort_key).bind(&date)
            .bind(date_precision.as_ref().map(DatePrecision::as_str)).bind(id)
            .execute(&mut *transaction).await.map_err(map_database)?;
        transaction.commit().await?;
        Ok(Unit {
            id: id.into(),
            edition_id: current.edition_id,
            label,
            kind,
            sort_key,
            date,
            date_precision,
        })
    }

    pub async fn list_units(&self, edition_id: &str) -> Result<Vec<Unit>, CatalogError> {
        sqlx::query("SELECT id, edition_id, label, kind, sort_key, date, date_precision FROM units WHERE edition_id = ? ORDER BY COALESCE(sort_key, label), id").bind(edition_id).fetch_all(self.store.reader()).await?.into_iter().map(unit_row).collect()
    }

    pub async fn list_units_page(
        &self,
        edition_id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Unit>, CatalogError> {
        self.list_units_page_search(edition_id, limit, cursor, None)
            .await
    }

    pub async fn list_units_page_search(
        &self,
        edition_id: &str,
        limit: u32,
        cursor: Option<&str>,
        q: Option<&str>,
    ) -> Result<Page<Unit>, CatalogError> {
        self.list_units_filtered(edition_id, limit, cursor, q, None)
            .await
    }

    pub async fn list_units_filtered(
        &self,
        edition_id: &str,
        limit: u32,
        cursor: Option<&str>,
        q: Option<&str>,
        kind: Option<UnitKind>,
    ) -> Result<Page<Unit>, CatalogError> {
        let q = normalize_search(q)?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid(
                "limit must be between 1 and 100".into(),
            ));
        }
        let cursor: Option<UnitCursor> = cursor.map(decode_cursor).transpose()?;
        if cursor.as_ref().is_some_and(|value| {
            value.edition_id != edition_id || value.q != q || value.kind != kind
        }) {
            return Err(CatalogError::Invalid(
                "cursor does not match edition".into(),
            ));
        }
        let kind_name = kind.as_ref().map(UnitKind::as_str);
        let rows = match &cursor {
            Some(cursor) => sqlx::query("SELECT id, edition_id, label, kind, sort_key, date, date_precision FROM units WHERE (? IS NULL OR instr(lower(label), lower(?)) > 0) AND (? IS NULL OR kind = ?) AND (edition_id = ? AND (COALESCE(sort_key, label) > ? OR (COALESCE(sort_key, label) = ? AND id > ?))) ORDER BY COALESCE(sort_key, label), id LIMIT ?").bind(q.as_deref()).bind(q.as_deref().unwrap_or("")).bind(kind_name).bind(kind_name).bind(edition_id).bind(&cursor.sort).bind(&cursor.sort).bind(&cursor.id).bind(i64::from(limit) + 1).fetch_all(self.store.reader()).await?,
            None => sqlx::query("SELECT id, edition_id, label, kind, sort_key, date, date_precision FROM units WHERE (? IS NULL OR instr(lower(label), lower(?)) > 0) AND (? IS NULL OR kind = ?) AND (edition_id = ?) ORDER BY COALESCE(sort_key, label), id LIMIT ?").bind(q.as_deref()).bind(q.as_deref().unwrap_or("")).bind(kind_name).bind(kind_name).bind(edition_id).bind(i64::from(limit) + 1).fetch_all(self.store.reader()).await?,
        };
        let mut items: Vec<_> = rows.into_iter().map(unit_row).collect::<Result<_, _>>()?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            let last = items.last().expect("a nonempty page has a final item");
            Some(encode_cursor(&UnitCursor {
                q,
                kind,
                edition_id: edition_id.into(),
                sort: last.sort_key.clone().unwrap_or_else(|| last.label.clone()),
                id: last.id.clone(),
            }))
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn create_provider_link(
        &self,
        input: NewProviderLink,
    ) -> Result<ProviderLink, CatalogError> {
        identity::required(&input.provider, "provider")?;
        identity::required(&input.external_id, "external_id")?;
        if [
            input.publication_id.is_some(),
            input.edition_id.is_some(),
            input.unit_id.is_some(),
        ]
        .into_iter()
        .filter(|value| *value)
        .count()
            != 1
        {
            return Err(CatalogError::Invalid(
                "provider link requires exactly one target".into(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("INSERT INTO provider_links (id, provider, external_id, publication_id, edition_id, unit_id) VALUES (?, ?, ?, ?, ?, ?)").bind(&id).bind(&input.provider).bind(&input.external_id).bind(&input.publication_id).bind(&input.edition_id).bind(&input.unit_id).execute(&mut *transaction).await.map_err(map_database)?;
        transaction.commit().await?;
        Ok(ProviderLink {
            id,
            provider: input.provider,
            external_id: input.external_id,
            publication_id: input.publication_id,
            edition_id: input.edition_id,
            unit_id: input.unit_id,
        })
    }
}

// Cover order matches the edition and unit list endpoints, then file id.
/// Matches a nonzero `file_count` in AVAILABILITY_SQL.
const HAS_FILES_SQL: &str = "SELECT 1 FROM editions e JOIN units u ON u.edition_id = e.id JOIN file_coverage c ON c.unit_id = u.id WHERE e.publication_id = p.id";

/// Page-bounded coverage for the publication ids bound as a JSON array. `position = 1` is the
/// cover: first file in edition order, then unit order. Shared so every cover agrees.
macro_rules! coverage_cte {
    () => {
        r#"
WITH ids(id) AS (SELECT DISTINCT value FROM json_each(?)),
coverage AS (
    SELECT e.publication_id, u.id AS unit_id, c.library_file_id,
        ROW_NUMBER() OVER (
            PARTITION BY e.publication_id
            ORDER BY e.language, COALESCE(e.region, ''), e.id, COALESCE(u.sort_key, u.label), u.id, c.library_file_id
        ) AS position
    FROM ids
    JOIN editions e ON e.publication_id = ids.id
    JOIN units u ON u.edition_id = e.id
    JOIN file_coverage c ON c.unit_id = u.id
),
"#
    };
}

const COVER_SQL: &str = concat!(
    coverage_cte!(),
    "covers AS (SELECT publication_id, library_file_id FROM coverage WHERE position = 1)\n",
    "SELECT publication_id, library_file_id FROM covers"
);

const AVAILABILITY_SQL: &str = concat!(
    coverage_cte!(),
    r#"
files AS (
    SELECT publication_id, COUNT(DISTINCT library_file_id) AS file_count,
        MAX(CASE WHEN position = 1 THEN library_file_id END) AS cover_file_id
    FROM coverage GROUP BY publication_id
),
monitored AS (
    SELECT e.publication_id, COUNT(DISTINCT m.unit_id) AS monitored_unit_count
    FROM ids
    JOIN editions e ON e.publication_id = ids.id
    JOIN units u ON u.edition_id = e.id
    JOIN monitors m ON m.unit_id = u.id
    WHERE m.owner = ? AND m.enabled = 1 AND m.deleted_at IS NULL
    GROUP BY e.publication_id
)
SELECT ids.id, COALESCE(files.file_count, 0) AS file_count,
    COALESCE(monitored.monitored_unit_count, 0) AS monitored_unit_count, files.cover_file_id
FROM ids
LEFT JOIN files ON files.publication_id = ids.id
LEFT JOIN monitored ON monitored.publication_id = ids.id
"#
);

async fn publication_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> Result<Option<Publication>, CatalogError> {
    let row = sqlx::query("SELECT id, content_type, title, sort_title, run_label, title_locked, known_unit_count FROM publications WHERE id = ?").bind(id).fetch_optional(&mut **transaction).await?;
    row.map(publication_row).transpose()
}
async fn edition_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> Result<Option<Edition>, CatalogError> {
    let row = sqlx::query(
        "SELECT id, publication_id, language, region, publisher FROM editions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(edition_row).transpose()
}
async fn unit_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> Result<Option<Unit>, CatalogError> {
    let row = sqlx::query("SELECT id, edition_id, label, kind, sort_key, date, date_precision FROM units WHERE id = ?").bind(id).fetch_optional(&mut **transaction).await?;
    row.map(unit_row).transpose()
}
fn publication_row(row: sqlx::sqlite::SqliteRow) -> Result<Publication, CatalogError> {
    Ok(Publication {
        id: row.try_get("id")?,
        content_type: ContentType::parse(row.try_get("content_type")?)
            .map_err(CatalogError::Invalid)?,
        title: row.try_get("title")?,
        sort_title: row.try_get("sort_title")?,
        run_label: row.try_get("run_label")?,
        title_locked: row.try_get::<i64, _>("title_locked")? != 0,
        known_unit_count: row.try_get("known_unit_count")?,
    })
}
fn edition_row(row: sqlx::sqlite::SqliteRow) -> Result<Edition, CatalogError> {
    Ok(Edition {
        id: row.try_get("id")?,
        publication_id: row.try_get("publication_id")?,
        language: row.try_get("language")?,
        region: row.try_get("region")?,
        publisher: row.try_get("publisher")?,
    })
}
fn unit_row(row: sqlx::sqlite::SqliteRow) -> Result<Unit, CatalogError> {
    Ok(Unit {
        id: row.try_get("id")?,
        edition_id: row.try_get("edition_id")?,
        label: row.try_get("label")?,
        kind: UnitKind::parse(row.try_get("kind")?).map_err(CatalogError::Invalid)?,
        sort_key: row.try_get("sort_key")?,
        date: row.try_get("date")?,
        date_precision: row
            .try_get::<Option<String>, _>("date_precision")?
            .map(DatePrecision::parse)
            .transpose()
            .map_err(CatalogError::Invalid)?,
    })
}
// SQLite lower() folds ASCII only; non-ASCII text remains an exact substring match.
fn normalize_search(q: Option<&str>) -> Result<Option<String>, CatalogError> {
    let Some(q) = q else { return Ok(None) };
    if q.len() > 256 || q.chars().any(char::is_control) {
        return Err(CatalogError::Invalid(
            "q must be at most 256 UTF-8 bytes and contain no control characters".into(),
        ));
    }
    let q = q.trim().to_ascii_lowercase();
    Ok((!q.is_empty()).then_some(q))
}

fn encode_cursor<T: Serialize>(cursor: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("cursor is serializable"))
}
fn decode_cursor<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, CatalogError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| CatalogError::Invalid("invalid cursor".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| CatalogError::Invalid("invalid cursor".into()))
}
fn map_database(error: sqlx::Error) -> CatalogError {
    if error.as_database_error().is_some_and(|database| {
        database.is_unique_violation() || database.is_foreign_key_violation()
    }) {
        CatalogError::Conflict
    } else {
        CatalogError::Database(error)
    }
}
