use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Executor, Sqlite, SqliteConnection, SqlitePool, Transaction};
use thiserror::Error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

const DATABASE_NAME: &str = "library.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot create or access state directory")]
    StateDirectory(#[source] std::io::Error),
    #[error("database schema is newer than this service supports")]
    NewerSchema,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct SqliteStore {
    writer: SqlitePool,
    reader: SqlitePool,
}

impl SqliteStore {
    pub async fn ensure_state_directory(state_dir: &Path) -> Result<(), StoreError> {
        let mut directory = tokio::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        directory.mode(0o700);
        directory
            .create(state_dir)
            .await
            .map_err(StoreError::StateDirectory)?;
        Self::check_state_directory(state_dir).await
    }

    async fn check_state_directory(state_dir: &Path) -> Result<(), StoreError> {
        let metadata = tokio::fs::symlink_metadata(state_dir)
            .await
            .map_err(StoreError::StateDirectory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(StoreError::StateDirectory(std::io::Error::other(
                "state path must be a real directory",
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o777 != 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(StoreError::StateDirectory(std::io::Error::other(
                    "state directory must be service-owned with mode 0700",
                )));
            }
        }
        Ok(())
    }

    /// Opens only an existing database without creating files or applying migrations.
    pub async fn open_existing_read_only(state_dir: &Path) -> Result<SqliteConnection, StoreError> {
        Self::check_state_directory(state_dir).await?;
        let database_path = state_dir.join(DATABASE_NAME);
        if !tokio::fs::symlink_metadata(&database_path)
            .await?
            .file_type()
            .is_file()
        {
            return Err(std::io::Error::other("database path must be a regular file").into());
        }
        Self::reject_newer_schema(&database_path).await?;
        Ok(SqliteConnection::connect_with(&Self::reader_options(&database_path)).await?)
    }

    pub async fn open(state_dir: &Path) -> Result<Self, StoreError> {
        Self::ensure_state_directory(state_dir).await?;
        let database_path = state_dir.join(DATABASE_NAME);
        let mut create = tokio::fs::OpenOptions::new();
        create.write(true).create_new(true);
        #[cfg(unix)]
        create.mode(0o600);
        match create.open(&database_path).await {
            Ok(file) => file.sync_all().await?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !tokio::fs::symlink_metadata(&database_path)
                    .await?
                    .file_type()
                    .is_file()
                {
                    return Err(
                        std::io::Error::other("database path must be a regular file").into(),
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }

        if database_path.exists() {
            Self::reject_newer_schema(&database_path).await?;
        }

        let writer_options = Self::writer_options(&database_path);
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    connection.execute("PRAGMA foreign_keys = ON").await?;
                    connection.execute("PRAGMA busy_timeout = 5000").await?;
                    Ok(())
                })
            })
            .connect_with(writer_options)
            .await?;
        MIGRATOR.run(&writer).await?;

        let reader_options = Self::reader_options(&database_path);
        let reader = SqlitePoolOptions::new()
            .max_connections(4)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    connection.execute("PRAGMA foreign_keys = ON").await?;
                    connection.execute("PRAGMA busy_timeout = 5000").await?;
                    Ok(())
                })
            })
            .connect_with(reader_options)
            .await?;

        Ok(Self { writer, reader })
    }

    pub async fn begin_write(&self) -> Result<Transaction<'_, Sqlite>, sqlx::Error> {
        // Acquire the writer before reading so a competing process cannot stale the snapshot.
        self.writer.begin_with("BEGIN IMMEDIATE").await
    }

    pub fn reader(&self) -> &SqlitePool {
        &self.reader
    }

    pub async fn close(&self) {
        self.reader.close().await;
        self.writer.close().await;
    }

    fn writer_options(database_path: &Path) -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
    }

    fn reader_options(database_path: &Path) -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .filename(database_path)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
    }

    async fn reject_newer_schema(database_path: &Path) -> Result<(), StoreError> {
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(database_path)
                .read_only(true),
        )
        .await?;
        let migrations_table_exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_sqlx_migrations'",
        )
        .fetch_optional(&mut connection)
        .await?;

        if migrations_table_exists.is_some() {
            let newest_applied: Option<i64> =
                sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                    .fetch_one(&mut connection)
                    .await?;
            let newest_supported = MIGRATOR.iter().map(|migration| migration.version).max();
            if newest_applied > newest_supported {
                return Err(StoreError::NewerSchema);
            }
        }

        connection.close().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "sqlite_test.rs"]
mod tests;
