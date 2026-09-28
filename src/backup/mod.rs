//! Private, consistent SQLite snapshots. Media files are backed up separately.
mod files;

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use sqlx::{Connection, Row};

use crate::{
    settings::{EncryptionKey, Settings, SettingsError},
    store::sqlite::{SqliteStore, StoreError},
};

use files::{copy_private, digest, private_directory, private_file, sync_directory};

const DATABASE: &str = "library.sqlite3";
const KEY: &str = "encryption.key";
const MANIFEST: &str = "manifest.json";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("backup database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("backup schema is incompatible or migrations failed")]
    Store(#[from] StoreError),
    #[error("backup credentials cannot be authenticated with the saved key")]
    Settings(#[from] SettingsError),
    #[error("invalid or incomplete backup: {0}")]
    Invalid(&'static str),
    #[error("backup manifest is invalid")]
    Manifest(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    service_version: String,
    database_sha256: String,
    key_sha256: Option<String>,
}

/// Destination must not exist; its parent must exist on local storage.
/// `destination/snapshot` appears only after all files have been validated and synced.
pub async fn backup(
    store: &SqliteStore,
    state_dir: &Path,
    destination: &Path,
) -> Result<(), BackupError> {
    verify_store(store, state_dir).await?;
    backup_existing(state_dir, destination).await
}

/// Snapshots an existing state database without creating files or migrating it.
pub async fn backup_existing(state_dir: &Path, destination: &Path) -> Result<(), BackupError> {
    let mut source = SqliteStore::open_existing_read_only(state_dir).await?;
    let result = backup_connection(&mut source, state_dir, destination).await;
    source.close().await?;
    result
}

async fn verify_store(store: &SqliteStore, state_dir: &Path) -> Result<(), BackupError> {
    let source = state_dir.join(DATABASE).canonicalize()?;
    let rows = sqlx::query("PRAGMA database_list")
        .fetch_all(store.reader())
        .await?;
    let main = rows
        .iter()
        .find(|row| row.get::<String, _>("name") == "main")
        .ok_or(BackupError::Invalid("store has no main database"))?;
    if Path::new(&main.get::<String, _>("file")).canonicalize()? != source {
        return Err(BackupError::Invalid(
            "state directory does not belong to store",
        ));
    }
    Ok(())
}

async fn backup_connection(
    source: &mut sqlx::SqliteConnection,
    state_dir: &Path,
    destination: &Path,
) -> Result<(), BackupError> {
    private_directory(destination)?;
    // Keep failures private and unpublished for inspection. Never remove user paths.
    let stage = destination.join(".pending");
    private_directory(&stage)?;
    let has_key = copy_key(state_dir, &stage)?;
    let database = stage.join(DATABASE);
    private_file(&database)?;
    let absolute = database.canonicalize()?;
    let filename = absolute
        .to_str()
        .ok_or(BackupError::Invalid("non-UTF-8 destination"))?;
    // The reader is read-only, but VACUUM INTO writes only its separate output file.
    // No BEGIN IMMEDIATE: VACUUM cannot run inside a transaction.
    sqlx::query("VACUUM INTO ?")
        .bind(filename)
        .execute(&mut *source)
        .await?;
    validate_staged(&stage, has_key).await?;
    let manifest = Manifest {
        format: 1,
        service_version: env!("CARGO_PKG_VERSION").to_owned(),
        database_sha256: digest(&database)?,
        key_sha256: has_key.then(|| digest(&stage.join(KEY))).transpose()?,
    };
    use std::io::Write;
    let mut file = private_file(&stage.join(MANIFEST))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    file.sync_all()?;
    files::open_private(&database)?.sync_all()?;
    sync_directory(&stage)?;
    // Both paths are inside a directory exclusively created by this invocation.
    fs::rename(&stage, destination.join("snapshot"))?;
    sync_directory(destination)?;
    sync_directory(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    Ok(())
}

async fn validate_staged(stage: &Path, has_key: bool) -> Result<(), BackupError> {
    let validation = stage
        .parent()
        .ok_or(BackupError::Invalid("invalid backup stage"))?
        .join(".validation");
    private_directory(&validation)?;
    copy_private(&stage.join(DATABASE), &validation.join(DATABASE))?;
    if has_key {
        copy_private(&stage.join(KEY), &validation.join(KEY))?;
    }
    validate(&validation, has_key).await?;
    fs::remove_dir_all(validation)?;
    Ok(())
}

/// Restore into a directory that does not exist. Never open or overwrite live state.
/// On failure the new directory is retained for inspection; do not start it.
pub async fn restore(backup_dir: &Path, fresh_state_dir: &Path) -> Result<(), BackupError> {
    files::check_directory(backup_dir)?;
    let snapshot = backup_dir.join("snapshot");
    files::check_directory(&snapshot)?;
    let file = files::open_private(&snapshot.join(MANIFEST))?;
    if file.metadata()?.len() > 4096 {
        return Err(BackupError::Invalid("oversized manifest"));
    }
    let manifest: Manifest = serde_json::from_reader(file)?;
    if manifest.format != 1 {
        return Err(BackupError::Invalid("unsupported format"));
    }
    private_directory(fresh_state_dir)?;
    let database = fresh_state_dir.join(DATABASE);
    copy_private(&snapshot.join(DATABASE), &database)?;
    if digest(&database)? != manifest.database_sha256 {
        return Err(BackupError::Invalid("database checksum mismatch"));
    }
    let has_key = copy_key(&snapshot, fresh_state_dir)?;
    let actual_key = has_key
        .then(|| digest(&fresh_state_dir.join(KEY)))
        .transpose()?;
    if actual_key != manifest.key_sha256 {
        return Err(BackupError::Invalid("key identity mismatch"));
    }
    validate(fresh_state_dir, has_key).await?;
    files::open_private(&database)?.sync_all()?;
    sync_directory(fresh_state_dir)?;
    sync_directory(
        fresh_state_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    Ok(())
}

fn copy_key(source: &Path, destination: &Path) -> Result<bool, BackupError> {
    let path = source.join(KEY);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.len() != 32 {
                return Err(BackupError::Invalid("invalid master key length"));
            }
            copy_private(&path, &destination.join(KEY))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn validate(directory: &Path, has_key: bool) -> Result<(), BackupError> {
    // The real startup path rejects newer migration versions and verifies checksums.
    let store = SqliteStore::open(directory).await?;
    let result = async {
        let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(store.reader())
            .await?;
        if integrity != ["ok"] {
            return Err(BackupError::Invalid("SQLite integrity check failed"));
        }
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.reader())
            .await?
            .is_empty()
        {
            return Err(BackupError::Invalid("foreign key check failed"));
        }
        let encrypted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM integrations")
            .fetch_one(store.reader())
            .await?;
        if !has_key && encrypted != 0 {
            return Err(BackupError::Invalid(
                "encrypted settings require the master key",
            ));
        }
        if has_key {
            // Only call load_or_create after verifying a saved key exists. Never replace it.
            let key = EncryptionKey::load_or_create(directory).await?;
            Settings::new(store.clone(), key).list().await?;
        }
        Ok(())
    }
    .await;
    store.close().await;
    result
}
