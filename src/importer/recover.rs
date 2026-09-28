use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::AsRawFd,
    path::PathBuf,
    sync::Arc,
};

use sqlx::Row;

use super::{
    finalize,
    journal::{ImportError, ImportOperation, ImportPhase, ImportService},
};

impl ImportService {
    /// Execute at most one durable phase. Dropping the caller does not release a
    /// filesystem lock while a blocking copy or decoder is still running.
    pub async fn step(&self, operation_id: &str) -> Result<ImportOperation, ImportError> {
        let service = self.clone();
        let id = operation_id.to_string();
        tokio::spawn(async move {
            let result = service.step_owned(&id).await;
            if let Err(error) = &result
                && !matches!(error, ImportError::Busy | ImportError::NotFound)
            {
                service.record_error(&id, error).await?;
            }
            result
        })
        .await
        .map_err(|_| ImportError::Worker)?
    }

    async fn step_owned(&self, id: &str) -> Result<ImportOperation, ImportError> {
        let initial = self.get(id).await?;
        let _lock = self.blocking(move || finalize::lock(&initial)).await?;
        let operation = self.get(id).await?;
        if self.root_path(&operation.request.source_root).await? != operation.identity.source_root
            || self.root_path(&operation.request.destination_root).await?
                != operation.identity.destination_root
        {
            return Err(ImportError::Changed);
        }
        match operation.phase {
            ImportPhase::Planned => {
                let op = operation.clone();
                let (identity, policy) = self.blocking(move || finalize::stage(&op)).await?;
                self.advance(
                    &operation,
                    ImportPhase::Staged,
                    Some(&identity),
                    Some(policy),
                    None,
                )
                .await?;
            }
            ImportPhase::Staged => {
                let op = operation.clone();
                let file = self.blocking(move || finalize::verify_stage(&op)).await?;
                let validation = self.validate(file, &operation).await?;
                self.advance(
                    &operation,
                    ImportPhase::Verified,
                    None,
                    None,
                    Some(validation),
                )
                .await?;
            }
            ImportPhase::Verified => {
                // Older journals may contain only the PDF header check. Upgrade that
                // evidence before publishing a file recovered from the verified phase.
                if operation.identity.format == "pdf"
                    && operation.validation.as_deref() != Some("pdf_manifest_verified")
                {
                    let op = operation.clone();
                    let file = self.blocking(move || finalize::verify_stage(&op)).await?;
                    let validation = self.validate(file, &operation).await?;
                    self.advance(
                        &operation,
                        ImportPhase::Verified,
                        None,
                        None,
                        Some(validation),
                    )
                    .await?;
                }
                let op = operation.clone();
                self.blocking(move || finalize::destination(&op)?.publish(&op))
                    .await?;
                self.advance(&operation, ImportPhase::Finalized, None, None, None)
                    .await?;
            }
            ImportPhase::Finalized => {
                let op = operation.clone();
                let (file, mtime) = self
                    .blocking(move || finalize::destination(&op)?.verify_final(&op))
                    .await?;
                self.validate(file, &operation).await?;
                self.catalog(&operation, mtime).await?;
            }
            ImportPhase::Cataloged => {
                self.verify_completed(&operation).await?;
                self.advance(&operation, ImportPhase::CleanupPending, None, None, None)
                    .await?;
            }
            ImportPhase::CleanupPending => {
                self.verify_completed(&operation).await?;
                let op = operation.clone();
                self.blocking(move || finalize::destination(&op)?.cleanup(&op))
                    .await?;
                self.advance(&operation, ImportPhase::Done, None, None, None)
                    .await?;
            }
            ImportPhase::Done => {
                self.verify_completed(&operation).await?;
                if operation.last_error.is_some() {
                    self.advance(&operation, ImportPhase::Done, None, None, None)
                        .await?;
                }
            }
        }
        self.get(id).await
    }

    /// Reconcile this operation through completion. Errors retain the current phase
    /// and a visible reason; a retry never chooses a second destination.
    pub async fn recover(&self, operation_id: &str) -> Result<ImportOperation, ImportError> {
        loop {
            let operation = self.step(operation_id).await?;
            if operation.phase == ImportPhase::Done {
                return Ok(operation);
            }
        }
    }

    /// A failure on one operation does not prevent recovery of the others.
    pub async fn recover_pending(
        &self,
    ) -> Result<Vec<(String, Result<ImportOperation, ImportError>)>, ImportError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM import_operations WHERE phase != 'done' ORDER BY created_at, id",
        )
        .fetch_all(self.store.reader())
        .await?;
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            let result = self.recover(&id).await;
            outcomes.push((id, result));
        }
        Ok(outcomes)
    }

    async fn validate(
        &self,
        mut file: File,
        operation: &ImportOperation,
    ) -> Result<&'static str, ImportError> {
        // The child opens this process's pinned descriptor, never a mutable client path.
        let path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        ));
        let label = match operation.identity.format.as_str() {
            "pdf" => {
                file.seek(SeekFrom::Start(0))?;
                let mut signature = [0; 8];
                file.read_exact(&mut signature)
                    .map_err(|_| ImportError::InvalidFormat)?;
                if &signature[..5] != b"%PDF-"
                    || !signature[5].is_ascii_digit()
                    || signature[6] != b'.'
                    || !signature[7].is_ascii_digit()
                {
                    return Err(ImportError::InvalidFormat);
                }
                // Duplicate the pinned descriptor, never reopen the source or staging path.
                // Retain our descriptor for the post-decode content check below.
                self.pdf_decoder
                    .manifest(Arc::new(file.try_clone()?))
                    .await
                    .map_err(|_| ImportError::InvalidFormat)?;
                "pdf_manifest_verified"
            }
            "cbz" | "cbr" => {
                file.seek(SeekFrom::Start(0))?;
                let mut magic = [0; 8];
                file.read_exact(&mut magic)
                    .map_err(|_| ImportError::InvalidFormat)?;
                let valid = if operation.identity.format == "cbz" {
                    magic.starts_with(b"PK\x03\x04")
                } else {
                    magic.starts_with(b"Rar!\x1a\x07\x00") || magic == *b"Rar!\x1a\x07\x01\x00"
                };
                if !valid {
                    return Err(ImportError::InvalidFormat);
                }
                let manifest = self
                    .decoder
                    .manifest(&path)
                    .await
                    .map_err(|_| ImportError::InvalidFormat)?;
                for page in manifest.pages {
                    self.decoder
                        .page(&path, &page.name)
                        .await
                        .map_err(|_| ImportError::InvalidFormat)?;
                }
                "archive_pages_verified"
            }
            _ => return Err(ImportError::InvalidFormat),
        };
        let intent = operation.identity.clone();
        self.blocking(move || finalize::check_content(&mut file, &intent))
            .await?;
        Ok(label)
    }

    async fn catalog(&self, operation: &ImportOperation, mtime: i64) -> Result<(), ImportError> {
        let path = operation
            .identity
            .destination_path(&operation.request)
            .to_string_lossy()
            .into_owned();
        let mut tx = self.store.begin_write().await?;
        let existing = sqlx::query("SELECT id, signature, size_bytes, format, root_id, relative_path FROM library_files WHERE path = ?")
            .bind(&path).fetch_optional(&mut *tx).await?;
        let file_id = match existing {
            Some(row) => {
                if row.get::<String, _>("signature") != operation.identity.signature
                    || row.get::<i64, _>("size_bytes") != operation.identity.size
                    || row.get::<String, _>("format") != operation.identity.format
                    || row.get::<Option<String>, _>("root_id").as_deref()
                        != Some(&operation.request.destination_root)
                    || row.get::<Option<String>, _>("relative_path").as_deref()
                        != Some(&operation.request.destination_relative)
                {
                    return Err(ImportError::Conflict);
                }
                row.get::<String, _>("id")
            }
            None => {
                // The journal UUID also makes the catalog identity deterministic on replay.
                let file_id = operation.id.clone();
                sqlx::query("INSERT INTO library_files (id, path, format, signature, size_bytes, root_id, relative_path, mtime_ns) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(&file_id).bind(&path).bind(&operation.identity.format).bind(&operation.identity.signature)
                    .bind(operation.identity.size).bind(&operation.request.destination_root).bind(&operation.request.destination_relative).bind(mtime)
                    .execute(&mut *tx).await?;
                file_id
            }
        };
        sqlx::query("INSERT INTO file_coverage (library_file_id, unit_id, evidence) VALUES (?, ?, 'user_confirmed') ON CONFLICT(library_file_id, unit_id) DO NOTHING")
            .bind(&file_id).bind(&operation.request.unit_id).execute(&mut *tx).await?;
        let result = sqlx::query("UPDATE import_operations SET phase = 'cataloged', library_file_id = ?, last_error = NULL, updated_at = unixepoch() WHERE id = ? AND phase = 'finalized'")
            .bind(&file_id).bind(&operation.id).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(ImportError::Busy);
        }
        tx.commit().await?;
        Ok(())
    }

    async fn verify_completed(&self, operation: &ImportOperation) -> Result<(), ImportError> {
        let op = operation.clone();
        let (file, _) = self
            .blocking(move || finalize::destination(&op)?.verify_final(&op))
            .await?;
        self.validate(file, operation).await?;
        let valid: Option<i64> = sqlx::query_scalar("SELECT 1 FROM library_files f JOIN file_coverage c ON c.library_file_id = f.id WHERE f.id = ? AND c.unit_id = ? AND f.path = ? AND f.signature = ? AND f.size_bytes = ? AND f.format = ? AND f.root_id = ? AND f.relative_path = ?")
            .bind(&operation.library_file_id).bind(&operation.request.unit_id)
            .bind(operation.identity.destination_path(&operation.request).to_string_lossy().as_ref())
            .bind(&operation.identity.signature).bind(operation.identity.size).bind(&operation.identity.format)
            .bind(&operation.request.destination_root).bind(&operation.request.destination_relative)
            .fetch_optional(self.store.reader()).await?;
        if valid.is_none() {
            return Err(ImportError::InvalidJournal);
        }
        Ok(())
    }
}
