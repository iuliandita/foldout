use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use serde::Serialize;
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::store::sqlite::SqliteStore;

static HASH_SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

pub(crate) fn hash_slots() -> Arc<tokio::sync::Semaphore> {
    HASH_SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
}

#[derive(Clone)]
pub struct Library {
    pub(crate) store: SqliteStore,
    pub(crate) hash_slots: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Root {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryEntry {
    pub id: String,
    pub relative_path: String,
    pub format: Option<String>,
    pub size_bytes: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
    pub associated_unit_count: i64,
    /// At most ASSOCIATED_UNITS_LIMIT units in catalog order; associated_unit_count is the total.
    pub associated_units: Vec<AssociatedUnit>,
}

pub const ASSOCIATED_UNITS_LIMIT: i64 = 5;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AssociatedUnit {
    pub publication_id: String,
    pub publication_title: String,
    pub unit_id: String,
    pub unit_label: String,
    pub unit_kind: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryEntryPage {
    pub items: Vec<InventoryEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("library root must be an existing directory")]
    InvalidRoot,
    #[error("library root was not found")]
    RootNotFound,
    #[error("library record conflicts with existing data")]
    Conflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("library inventory I/O failed")]
    Io(#[from] std::io::Error),
}

impl Library {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            hash_slots: hash_slots(),
        }
    }

    pub async fn register_root(&self, label: &str, path: &Path) -> Result<Root, LibraryError> {
        if label.trim().is_empty() {
            return Err(LibraryError::InvalidRoot);
        }
        let path = canonical_directory(path).map_err(|_| LibraryError::InvalidRoot)?;
        let root = Root {
            id: Uuid::new_v4().to_string(),
            label: label.into(),
            path,
        };
        let mut transaction = self.store.begin_write().await?;
        let result = sqlx::query("INSERT INTO library_roots (id, label, path) VALUES (?, ?, ?)")
            .bind(&root.id)
            .bind(&root.label)
            .bind(root.path.to_string_lossy().as_ref())
            .execute(&mut *transaction)
            .await;
        match result {
            Ok(_) => transaction.commit().await?,
            Err(error)
                if error
                    .as_database_error()
                    .is_some_and(|value| value.is_unique_violation()) =>
            {
                return Err(LibraryError::Conflict);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(root)
    }

    pub async fn list_roots(&self) -> Result<Vec<Root>, LibraryError> {
        let rows = sqlx::query("SELECT id, label, path FROM library_roots ORDER BY label, id")
            .fetch_all(self.store.reader())
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| Root {
                id: row.get("id"),
                label: row.get("label"),
                path: PathBuf::from(row.get::<String, _>("path")),
            })
            .collect())
    }

    /// Lists associations current as of the inventory's last observation, without rescanning.
    pub async fn inventory_entries(
        &self,
        root_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<InventoryEntryPage, LibraryError> {
        self.root(root_id).await?;
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query(
            "SELECT e.id, e.relative_path, e.format, e.size_bytes, e.state, e.reason,
                (SELECT COUNT(*) FROM library_files f
                 JOIN file_coverage c ON c.library_file_id = f.id
                 WHERE e.state = 'pending_association'
                   AND f.path = rtrim(r.path, '/') || '/' || e.relative_path
                   AND f.signature = e.signature AND f.size_bytes = e.size_bytes
                ) AS associated_unit_count
             FROM scan_entries e JOIN library_roots r ON r.id = e.root_id
             WHERE e.root_id = ? AND e.id > ? ORDER BY e.id LIMIT ?",
        )
        .bind(root_id)
        .bind(cursor.unwrap_or_default())
        .bind(i64::from(limit) + 1)
        .fetch_all(self.store.reader())
        .await?;
        let mut items: Vec<InventoryEntry> = rows
            .into_iter()
            .map(|row| InventoryEntry {
                id: row.get("id"),
                relative_path: row.get("relative_path"),
                format: row.get("format"),
                size_bytes: row.get("size_bytes"),
                state: row.get("state"),
                reason: row.get("reason"),
                associated_unit_count: row.get("associated_unit_count"),
                associated_units: Vec::new(),
            })
            .collect();
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|entry| entry.id.clone())
        } else {
            None
        };
        let ids: Vec<&str> = items
            .iter()
            .filter(|entry| entry.associated_unit_count > 0)
            .map(|entry| entry.id.as_str())
            .collect();
        if !ids.is_empty() {
            // Same match predicate as associated_unit_count, bounded to this page.
            let rows = sqlx::query(
                "WITH linked AS (
                    SELECT e.id AS entry_id, p.id AS publication_id, p.title AS publication_title,
                        u.id AS unit_id, u.label AS unit_label, u.kind AS unit_kind,
                        ROW_NUMBER() OVER (
                            PARTITION BY e.id
                            ORDER BY p.sort_title, p.id, ed.id, COALESCE(u.sort_key, u.label), u.id
                        ) AS position
                    FROM scan_entries e
                    JOIN library_roots r ON r.id = e.root_id
                    JOIN library_files f ON f.path = rtrim(r.path, '/') || '/' || e.relative_path
                        AND f.signature = e.signature AND f.size_bytes = e.size_bytes
                    JOIN file_coverage c ON c.library_file_id = f.id
                    JOIN units u ON u.id = c.unit_id
                    JOIN editions ed ON ed.id = u.edition_id
                    JOIN publications p ON p.id = ed.publication_id
                    WHERE e.root_id = ? AND e.state = 'pending_association'
                      AND e.id IN (SELECT value FROM json_each(?))
                 )
                 SELECT * FROM linked WHERE position <= ? ORDER BY entry_id, position",
            )
            .bind(root_id)
            .bind(serde_json::to_string(&ids).expect("ids are serializable"))
            .bind(ASSOCIATED_UNITS_LIMIT)
            .fetch_all(self.store.reader())
            .await?;
            for row in rows {
                let entry_id: String = row.get("entry_id");
                if let Some(entry) = items.iter_mut().find(|entry| entry.id == entry_id) {
                    entry.associated_units.push(AssociatedUnit {
                        publication_id: row.get("publication_id"),
                        publication_title: row.get("publication_title"),
                        unit_id: row.get("unit_id"),
                        unit_label: row.get("unit_label"),
                        unit_kind: row.get("unit_kind"),
                    });
                }
            }
        }
        Ok(InventoryEntryPage { items, next_cursor })
    }

    pub(crate) async fn root(&self, id: &str) -> Result<Root, LibraryError> {
        let row = sqlx::query("SELECT id, label, path FROM library_roots WHERE id = ?")
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?;
        row.map(|row| Root {
            id: row.get("id"),
            label: row.get("label"),
            path: PathBuf::from(row.get::<String, _>("path")),
        })
        .ok_or(LibraryError::RootNotFound)
    }
}

pub(crate) fn canonical_directory(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(std::io::Error::other("not a directory"))
    }
}
