use std::{path::PathBuf, sync::Arc};

use serde::Serialize;
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    library::{
        LibraryFile,
        roots::{canonical_directory, hash_slots},
        scan::{HashResult, hash_safe},
    },
    store::sqlite::SqliteStore,
};

#[derive(Clone)]
pub struct PreviewService {
    store: SqliteStore,
    hash_slots: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportPreview {
    pub id: String,
    pub entry_id: String,
    pub unit_id: String,
    pub signature: String,
    pub source_size: i64,
    pub state: String,
}

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("inventory entry was not found or is not ready for association")]
    EntryNotFound,
    #[error("catalog unit was not found")]
    UnitNotFound,
    #[error("preview was not found")]
    NotFound,
    #[error("preview source changed and must be reviewed again")]
    Stale,
    #[error("a different file already owns this path")]
    Conflict,
    #[error("source cannot be safely reopened")]
    UnsafeSource,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl PreviewService {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            hash_slots: hash_slots(),
        }
    }

    pub async fn preview(
        &self,
        entry_id: &str,
        unit_id: &str,
    ) -> Result<ImportPreview, PreviewError> {
        let entry = sqlx::query("SELECT signature, size_bytes, mtime_ns FROM scan_entries WHERE id = ? AND state = 'pending_association'").bind(entry_id).fetch_optional(self.store.reader()).await?.ok_or(PreviewError::EntryNotFound)?;
        let unit_exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM units WHERE id = ?")
            .bind(unit_id)
            .fetch_optional(self.store.reader())
            .await?;
        if unit_exists.is_none() {
            return Err(PreviewError::UnitNotFound);
        }
        let preview = ImportPreview {
            id: Uuid::new_v4().to_string(),
            entry_id: entry_id.into(),
            unit_id: unit_id.into(),
            signature: entry.get::<String, _>("signature"),
            source_size: entry.get("size_bytes"),
            state: "pending".into(),
        };
        let mut transaction = self.store.begin_write().await?;
        sqlx::query("INSERT INTO import_previews (id, entry_id, unit_id, signature, source_size, source_mtime_ns, state, created_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', unixepoch())")
            .bind(&preview.id).bind(&preview.entry_id).bind(&preview.unit_id).bind(&preview.signature).bind(preview.source_size).bind(entry.get::<i64, _>("mtime_ns")).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(preview)
    }

    pub async fn accept(&self, preview_id: &str) -> Result<LibraryFile, PreviewError> {
        let row = sqlx::query("SELECT p.id, p.entry_id, p.unit_id, p.signature, p.source_size, p.source_mtime_ns, p.state, e.root_id, e.relative_path, e.format, r.path AS root_path FROM import_previews p JOIN scan_entries e ON e.id = p.entry_id JOIN library_roots r ON r.id = e.root_id WHERE p.id = ?").bind(preview_id).fetch_optional(self.store.reader()).await?.ok_or(PreviewError::NotFound)?;
        let accepted = row.get::<String, _>("state") == "accepted";
        let registered_root = PathBuf::from(row.get::<String, _>("root_path"));
        let root = canonical_directory(&registered_root).map_err(|_| PreviewError::UnsafeSource)?;
        if root != registered_root {
            return Err(PreviewError::UnsafeSource);
        }
        let source = root.join(row.get::<String, _>("relative_path"));
        let hash_root = root.clone();
        let hash_source = source.clone();
        let permit = self
            .hash_slots
            .clone()
            .acquire_owned()
            .await
            .expect("hash semaphore remains open");
        let hashed: HashResult = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash_safe(&hash_root, &hash_source)
        })
        .await
        .map_err(|_| PreviewError::UnsafeSource)?
        .map_err(|_| PreviewError::UnsafeSource)?;
        if hashed.signature != row.get::<String, _>("signature")
            || hashed.size != row.get::<i64, _>("source_size")
        {
            self.stale(preview_id).await?;
            return Err(PreviewError::Stale);
        }
        if accepted {
            return self.accepted_file(preview_id).await;
        }
        let path = source.to_string_lossy().into_owned();
        let format = row
            .get::<String, _>("format")
            .parse()
            .map_err(|_| PreviewError::EntryNotFound)?;
        let file = LibraryFile {
            id: Uuid::new_v4().to_string(),
            path: path.clone(),
            format,
            signature: hashed.signature,
            size_bytes: hashed.size,
        };
        let mut transaction = self.store.begin_write().await?;
        let existing = sqlx::query(
            "SELECT id, path, format, signature, size_bytes FROM library_files WHERE path = ?",
        )
        .bind(&path)
        .fetch_optional(&mut *transaction)
        .await?;
        let file = match existing {
            Some(existing) if existing.get::<String, _>("signature") == file.signature => {
                library_file(existing)?
            }
            Some(_) => {
                sqlx::query("UPDATE import_previews SET state = 'conflict' WHERE id = ?")
                    .bind(preview_id)
                    .execute(&mut *transaction)
                    .await?;
                transaction.commit().await?;
                return Err(PreviewError::Conflict);
            }
            None => {
                sqlx::query("INSERT INTO library_files (id, path, format, signature, size_bytes, root_id, relative_path, mtime_ns) VALUES (?, ?, ?, ?, ?, ?, ?, ?)").bind(&file.id).bind(&file.path).bind(file.format.as_str()).bind(&file.signature).bind(file.size_bytes).bind(row.get::<String, _>("root_id")).bind(row.get::<String, _>("relative_path")).bind(hashed.mtime_ns).execute(&mut *transaction).await?;
                file
            }
        };
        sqlx::query("INSERT INTO file_coverage (library_file_id, unit_id, evidence) VALUES (?, ?, 'user_confirmed') ON CONFLICT(library_file_id, unit_id) DO NOTHING").bind(&file.id).bind(row.get::<String, _>("unit_id")).execute(&mut *transaction).await?;
        sqlx::query("UPDATE import_previews SET state = 'accepted', accepted_library_file_id = ? WHERE id = ?").bind(&file.id).bind(preview_id).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(file)
    }

    async fn stale(&self, id: &str) -> Result<(), PreviewError> {
        let mut tx = self.store.begin_write().await?;
        sqlx::query("UPDATE import_previews SET state = 'stale' WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn accepted_file(&self, id: &str) -> Result<LibraryFile, PreviewError> {
        let row = sqlx::query("SELECT f.id, f.path, f.format, f.signature, f.size_bytes FROM import_previews p JOIN library_files f ON f.id = p.accepted_library_file_id WHERE p.id = ?").bind(id).fetch_optional(self.store.reader()).await?.ok_or(PreviewError::NotFound)?;
        library_file(row)
    }
}

fn library_file(row: sqlx::sqlite::SqliteRow) -> Result<LibraryFile, PreviewError> {
    Ok(LibraryFile {
        id: row.get("id"),
        path: row.get("path"),
        format: row
            .get::<String, _>("format")
            .parse()
            .map_err(|_| PreviewError::EntryNotFound)?,
        signature: row.get("signature"),
        size_bytes: row.get("size_bytes"),
    })
}
