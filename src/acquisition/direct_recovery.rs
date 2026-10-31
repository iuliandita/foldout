use super::*;
use crate::acquisition::transfer_checkpoint::{Checkpoint, origin};

#[derive(Debug, Serialize)]
pub struct Eligibility {
    pub eligible: bool,
    pub reason: Option<String>,
}
impl Eligibility {
    fn from_reason(reason: Option<&str>) -> Self {
        Self {
            eligible: reason.is_none(),
            reason: reason.map(str::to_owned),
        }
    }
}
#[derive(Debug, Serialize)]
pub struct DirectReview {
    pub acquisition: DirectAcquisition,
    pub revision: i64,
    pub transfer_fingerprint: String,
    pub resume: Eligibility,
    pub restart: Eligibility,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferAction {
    pub revision: i64,
    pub transfer_fingerprint: String,
}
pub(super) struct ResolvedTransfer {
    pub download: Option<GetComicsDownload>,
    pub manifest: Option<MangaDexManifest>,
    pub language: Option<String>,
}
impl Direct {
    pub(super) async fn resolve_transfer(
        &self,
        settings: &Settings,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<ResolvedTransfer, DirectError> {
        let owner: String = row.try_get("owner")?;
        let handle: String = row.try_get("link_handle")?;
        let selection = self.selection(&owner, &handle).await?;
        if selection.source_kind == "mangadex_chapter" {
            let (provider, chapter) = self.selected_chapter(settings, &owner, &handle).await?;
            let manifest = provider.at_home(&chapter).await?;
            if manifest.summary().chapter_id != chapter.id
                || manifest.summary().page_count != chapter.page_count
            {
                return Err(DirectError::Changed);
            }
            Ok(ResolvedTransfer {
                download: None,
                manifest: Some(manifest),
                language: Some(chapter.language),
            })
        } else {
            let resolution = self.selected(settings, &owner, &handle).await?;
            let download = match resolution.summary.state {
                GetComicsLinkState::Direct => {
                    resolution.into_download().ok_or(DirectError::Changed)?
                }
                GetComicsLinkState::ManualAction => return Err(DirectError::ManualAction),
                _ => return Err(ProviderError::Unsupported.into()),
            };
            Ok(ResolvedTransfer {
                download: Some(download),
                manifest: None,
                language: None,
            })
        }
    }
    pub(super) async fn initialize_parts(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        acquisition_id: &str,
        identities: &[(u32, String)],
    ) -> Result<(), DirectError> {
        for (ordinal, identity) in identities {
            sqlx::query("INSERT INTO direct_transfer_parts(acquisition_id,ordinal,page_identity) VALUES(?,?,?)")
                .bind(acquisition_id).bind(ordinal).bind(identity).execute(&mut **tx).await?;
        }
        Ok(())
    }
    pub async fn review(
        &self,
        owner: &str,
        acquisition_id: &str,
    ) -> Result<DirectReview, DirectError> {
        let mut tx = self.store.reader().begin().await?;
        let row = review_row(&mut tx, owner, acquisition_id).await?;
        let revision = row.try_get("review_revision")?;
        let transfer_fingerprint = fingerprint_in(&mut tx, acquisition_id).await?;
        let blocked = blocked_in(&mut tx, &row).await?;
        let parts = parts_in(&mut tx, acquisition_id).await?;
        tx.commit().await?;
        let image = row.try_get::<Option<u32>, _>("expected_pages")?.is_some();
        let mut restart_reason = idle_reason(&row, blocked)?;
        if restart_reason.is_none() {
            let source = self
                .root(&row.try_get::<String, _>("download_root_id")?)
                .await?;
            if Stage::prepare_discard(
                &source,
                acquisition_id,
                row.try_get::<Option<&str>, _>("root_identity")?,
            )
            .is_err()
            {
                restart_reason = Some("local_review");
            }
        }
        let mut resume_reason = restart_reason.or_else(|| {
            if row.get::<Option<&str>, _>("reason") == Some("transfer_restart_required") {
                Some("transfer_restart_required")
            } else if !row.get::<bool, _>("attempted") {
                Some("transfer_evidence_missing")
            } else {
                None
            }
        });
        if resume_reason.is_none() && parts.iter().any(|p| p.invalid) {
            resume_reason = Some("transfer_checkpoint_invalid");
        }
        if resume_reason.is_none()
            && parts.iter().filter_map(|p| p.checkpoint.as_ref()).any(|c| {
                !c.complete
                    && !(image && c.unstarted())
                    && c.etag.is_none()
                    && c.last_modified.is_some()
                    && !c.last_modified_strong
            })
        {
            resume_reason = Some("transfer_validator_not_strong");
        }
        if resume_reason.is_none() {
            resume_reason = match self.check_parts(&row, &parts, None).await {
                Ok(_) => None,
                Err(DirectError::ResumeUnavailable) => Some("transfer_resume_unavailable"),
                Err(DirectError::RestartRequired | DirectError::LocalReview) => {
                    Some("transfer_restart_required")
                }
                Err(other) => return Err(other),
            };
        }
        Ok(DirectReview {
            acquisition: view(row)?,
            revision,
            transfer_fingerprint,
            resume: Eligibility::from_reason(resume_reason),
            restart: Eligibility::from_reason(restart_reason),
        })
    }

    pub async fn start_resume_transfer(
        &self,
        settings: &Settings,
        owner: &str,
        acquisition_id: &str,
        request: TransferAction,
    ) -> Result<DirectAcquisition, DirectError> {
        let direct = self.clone();
        let settings = settings.clone();
        let owner = owner.to_owned();
        let acquisition_id = acquisition_id.to_owned();
        let (send, ready) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut started = Some(send);
            let result = direct
                .resume_transfer_owned(&settings, &owner, &acquisition_id, request, &mut started)
                .await;
            if let Some(send) = started.take() {
                let _ = send.send(result);
            } else if let Err(error) = result {
                tracing::warn!(%error, "reviewed direct transfer stopped; inspect its current state");
            }
        });
        ready.await.map_err(|_| DirectError::LocalReview)?
    }
    #[cfg(test)]
    pub async fn resume_transfer(
        &self,
        settings: &Settings,
        owner: &str,
        acquisition_id: &str,
        request: TransferAction,
    ) -> Result<DirectAcquisition, DirectError> {
        self.resume_transfer_owned(settings, owner, acquisition_id, request, &mut None)
            .await
    }
    async fn resume_transfer_owned(
        &self,
        settings: &Settings,
        owner: &str,
        acquisition_id: &str,
        request: TransferAction,
        started: &mut Option<tokio::sync::oneshot::Sender<Result<DirectAcquisition, DirectError>>>,
    ) -> Result<DirectAcquisition, DirectError> {
        // Owner lookup precedes the global lock, preserving the scoped 404 contract.
        self.get(owner, acquisition_id).await?;
        let _slot = SLOT
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .try_lock()
            .map_err(|_| DirectError::Busy)?;
        let _process_lock = self.worker_lock().await?.ok_or(DirectError::Busy)?;
        let (row, parts, evidence) = self.prepare_action(owner, acquisition_id, &request).await?;
        if !row.try_get::<bool, _>("attempted")? {
            return Err(DirectError::ResumeUnavailable);
        }
        if row.try_get::<Option<&str>, _>("reason")? == Some("transfer_restart_required") {
            return Err(DirectError::RestartRequired);
        }
        let prepared = async {
            let resolved =
                self.resolve_transfer(settings, &row)
                    .await
                    .map_err(|error| match error {
                        DirectError::Changed => DirectError::RestartRequired,
                        other => other,
                    })?;
            let stage = self.check_parts(&row, &parts, Some(&resolved)).await?;
            let destination = self
                .root(&row.try_get::<String, _>("destination_root_id")?)
                .await?;
            let dest = finalize::root(&destination).map_err(|_| DirectError::LocalReview)?;
            absent_destination(&dest, &row.try_get::<String, _>("destination_relative")?)?;
            if identity(&dest)? != row.try_get::<String, _>("destination_identity")? {
                return Err(DirectError::LocalReview);
            }
            Ok((resolved, stage))
        }
        .await;
        let (resolved, stage) = match prepared {
            Ok(prepared) => prepared,
            Err(error @ (DirectError::RestartRequired | DirectError::ResumeUnavailable)) => {
                let mut tx = self.store.begin_write().await?;
                action_in(&mut tx, owner, acquisition_id, &request, Some(&evidence)).await?;
                sqlx::query(
                    "UPDATE direct_acquisitions SET reason=?,updated_at=unixepoch() WHERE id=?",
                )
                .bind(reason(&error))
                .bind(acquisition_id)
                .execute(&mut *tx)
                .await?;
                audit(&mut tx, acquisition_id, "resume", &request, None).await?;
                tx.commit().await?;
                return Err(error);
            }
            Err(other) => return Err(other),
        };
        let mut tx = self.store.begin_write().await?;
        let current = action_in(&mut tx, owner, acquisition_id, &request, Some(&evidence)).await?;
        check_unit(
            &mut tx,
            &current.try_get::<String, _>("unit_id")?,
            resolved.language.as_deref(),
        )
        .await?;
        let accepted = view(sqlx::query("UPDATE direct_acquisitions SET state='downloading',reason=NULL,updated_at=unixepoch() WHERE id=? RETURNING *")
            .bind(acquisition_id).fetch_one(&mut *tx).await?)?;
        audit(&mut tx, acquisition_id, "resume", &request, None).await?;
        tx.commit().await?;
        if let Some(send) = started.take() {
            let _ = send.send(Ok(accepted));
        }
        let result = self.finish_transfer(&row, resolved, stage, true).await;
        if let Err(error) = result {
            self.state(acquisition_id, "needs_review", Some(reason(&error)))
                .await?;
            return Err(error);
        }
        self.get(owner, acquisition_id).await
    }

    pub async fn restart_transfer(
        &self,
        settings: &Settings,
        owner: &str,
        acquisition_id: &str,
        request: TransferAction,
    ) -> Result<DirectAcquisition, DirectError> {
        self.get(owner, acquisition_id).await?;
        if let Some(replacement) = self.restart_replay(owner, acquisition_id, &request).await? {
            return Ok(replacement);
        }
        let _slot = SLOT
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .try_lock()
            .map_err(|_| DirectError::Busy)?;
        let _process_lock = self.worker_lock().await?.ok_or(DirectError::Busy)?;
        if let Some(replacement) = self.restart_replay(owner, acquisition_id, &request).await? {
            return Ok(replacement);
        }
        let (row, _, evidence) = self.prepare_action(owner, acquisition_id, &request).await?;
        let resolved = self.resolve_transfer(settings, &row).await?;
        let format = resolved
            .download
            .as_ref()
            .map(|d| archive_format(d.url()))
            .transpose()?
            .unwrap_or("cbz");
        let relative: String = row.try_get("destination_relative")?;
        if Path::new(&relative).extension().and_then(|s| s.to_str()) != Some(format) {
            return Err(DirectError::InvalidArchive);
        }
        let source = self
            .root(&row.try_get::<String, _>("download_root_id")?)
            .await?;
        let destination = self
            .root(&row.try_get::<String, _>("destination_root_id")?)
            .await?;
        let dest = finalize::root(&destination).map_err(|_| DirectError::LocalReview)?;
        absent_destination(&dest, &relative)?;
        let retired_stage = Stage::prepare_discard(
            &source,
            acquisition_id,
            row.try_get::<Option<&str>, _>("root_identity")?,
        )?;
        // Retire the original attempt. A fresh UUID cannot adopt its old partial or manifest.
        let replacement = Uuid::new_v4().to_string();
        let new_request = DirectRequest {
            link_handle: row.try_get("link_handle")?,
            unit_id: row.try_get("unit_id")?,
            download_root_id: row.try_get("download_root_id")?,
            destination: DirectDestination {
                root_id: row.try_get("destination_root_id")?,
                relative_path: relative,
            },
        };
        let digest = hash(&serde_json::to_vec(&new_request).map_err(|_| DirectError::Invalid)?);
        let mut tx = self.store.begin_write().await?;
        action_in(&mut tx, owner, acquisition_id, &request, Some(&evidence)).await?;
        check_unit(&mut tx, &new_request.unit_id, resolved.language.as_deref()).await?;
        sqlx::query("UPDATE direct_acquisitions SET state='canceled',canceled_at=unixepoch(),updated_at=unixepoch() WHERE id=?")
            .bind(acquisition_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state) VALUES(?,?,?,?,?,?,?,?,?,?,'queued')")
            .bind(&replacement).bind(owner).bind(format!("restart:{acquisition_id}:{}",request.revision)).bind(digest).bind(&new_request.link_handle).bind(&new_request.unit_id).bind(&new_request.download_root_id).bind(&new_request.destination.root_id).bind(&new_request.destination.relative_path).bind(Uuid::new_v4().to_string()).execute(&mut *tx).await?;
        audit(
            &mut tx,
            acquisition_id,
            "restart",
            &request,
            Some(&replacement),
        )
        .await?;
        tx.commit().await?;
        // Only the retired intent's private partials; never a published archive or library file.
        if let Some(stage) = retired_stage
            && let Err(error) = stage.discard(&source)
        {
            tracing::warn!(%error, "restart committed; retired partial cleanup needs local inspection");
        }
        self.get(owner, &replacement).await
    }
    async fn restart_replay(
        &self,
        owner: &str,
        id: &str,
        request: &TransferAction,
    ) -> Result<Option<DirectAcquisition>, DirectError> {
        let replacement: Option<String> = sqlx::query_scalar("SELECT e.replacement_id FROM direct_transfer_events e JOIN direct_acquisitions a ON a.id=e.acquisition_id WHERE e.acquisition_id=? AND a.owner=? AND e.action='restart' AND e.revision=? AND e.transfer_fingerprint=?")
            .bind(id).bind(owner).bind(request.revision).bind(&request.transfer_fingerprint).fetch_optional(self.store.reader()).await?.flatten();
        match replacement {
            Some(id) => Ok(Some(self.get(owner, &id).await?)),
            None => Ok(None),
        }
    }
    async fn prepare_action(
        &self,
        owner: &str,
        acquisition_id: &str,
        request: &TransferAction,
    ) -> Result<(sqlx::sqlite::SqliteRow, Vec<Part>, String), DirectError> {
        let mut tx = self.store.reader().begin().await?;
        let row = action_in(&mut tx, owner, acquisition_id, request, None).await?;
        let parts = parts_in(&mut tx, acquisition_id).await?;
        let evidence = evidence_in(&mut tx, acquisition_id).await?;
        tx.commit().await?;
        Ok((row, parts, evidence))
    }
    async fn check_parts(
        &self,
        row: &sqlx::sqlite::SqliteRow,
        parts: &[Part],
        resolved: Option<&ResolvedTransfer>,
    ) -> Result<Stage, DirectError> {
        let expected_pages: Option<u32> = row.try_get("expected_pages")?;
        let expected = expected_pages.map_or(1, |n| n as usize);
        if parts.len() != expected
            || parts.iter().enumerate().any(|(i, p)| {
                p.ordinal
                    != if expected_pages.is_some() {
                        i as u32 + 1
                    } else {
                        0
                    }
            })
        {
            return Err(DirectError::RestartRequired);
        }
        if let Some(resolved) = resolved
            && (resolved.manifest.as_ref().map(|m| m.identity())
                != row.try_get::<Option<&str>, _>("manifest_identity")?
                || resolved.manifest.as_ref().map(|m| m.summary().page_count) != expected_pages
                || part_identities(resolved.manifest.as_ref())?
                    != parts
                        .iter()
                        .map(|p| (p.ordinal, p.identity.clone()))
                        .collect::<Vec<_>>())
        {
            return Err(DirectError::RestartRequired);
        }
        let source = self
            .root(&row.try_get::<String, _>("download_root_id")?)
            .await?;
        let acquisition_id: String = row.try_get("id")?;
        let stage = Stage::open(&source, &acquisition_id, resolved.is_some())?;
        if identity(&stage.root)? != row.try_get::<String, _>("root_identity")? {
            return Err(DirectError::LocalReview);
        }
        stage.check(&source)?;
        if expected_pages.is_some() {
            check_page_names(&stage.job, parts)?;
        }
        for part in parts {
            if part.invalid {
                return Err(DirectError::RestartRequired);
            }
            if expected_pages.is_some() && part.fresh_page() {
                let name = page_name(part.ordinal);
                if page_absent(&stage.job, &name)? {
                    continue;
                }
                let file = open_at(&stage.job, &name, libc::O_RDONLY)?;
                let m = file.metadata().map_err(|_| DirectError::LocalReview)?;
                if !m.is_file()
                    || m.nlink() != 1
                    || m.uid() != unsafe { libc::geteuid() }
                    || m.mode() & 0o777 != 0o600
                {
                    return Err(DirectError::RestartRequired);
                }
                if m.len() != 0 {
                    // Only the action may truncate a header-only checkpoint's owned tail.
                    part.checkpoint
                        .as_ref()
                        .ok_or(DirectError::RestartRequired)?
                        .check_metadata(&file)?;
                }
                continue;
            }
            let checkpoint = part
                .checkpoint
                .as_ref()
                .ok_or(DirectError::ResumeUnavailable)?;
            if !checkpoint.eligible() && !(expected_pages.is_some() && checkpoint.complete) {
                return Err(DirectError::ResumeUnavailable);
            }
            let file = if part.ordinal == 0 {
                stage
                    .file
                    .try_clone()
                    .map_err(|_| DirectError::LocalReview)?
            } else {
                open_at(&stage.job, &page_name(part.ordinal), libc::O_RDONLY)?
            };
            // Review and preparation use cheap facts. The transfer verifies the prefix once,
            // on the blocking pool, before any GET or authorized tail truncation.
            checkpoint.check_metadata(&file)?;
            if let Some(resolved) = resolved
                && expected_pages.is_none()
            {
                let url = resolved
                    .download
                    .as_ref()
                    .ok_or(DirectError::RestartRequired)?
                    .url()
                    .clone();
                #[cfg(any(test, feature = "soak-fixtures"))]
                let url = if let Some(address) = self.fixture {
                    let mut target = reqwest::Url::parse(&format!("http://{address}/"))
                        .map_err(|_| DirectError::Invalid)?;
                    target.set_path(url.path());
                    target.set_query(url.query());
                    target
                } else {
                    url
                };
                if checkpoint.origin != origin(&url) {
                    return Err(DirectError::RestartRequired);
                }
            }
        }

        Ok(stage)
    }
}
struct Part {
    ordinal: u32,
    identity: String,
    checkpoint: Option<Checkpoint>,
    invalid: bool,
}
impl Part {
    fn fresh_page(&self) -> bool {
        !self.invalid && self.checkpoint.as_ref().is_none_or(Checkpoint::unstarted)
    }
}
async fn parts_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<Vec<Part>, DirectError> {
    sqlx::query("SELECT ordinal,page_identity,checkpoint FROM direct_transfer_parts WHERE acquisition_id=? ORDER BY ordinal")
        .bind(id).fetch_all(&mut **tx).await?.into_iter().map(|r| {
            let raw: Option<String> = r.try_get("checkpoint")?;
            let parsed = raw.as_ref().map(|s|serde_json::from_str::<Checkpoint>(s));
            let invalid = parsed.as_ref().is_some_and(|p|p.is_err());
            Ok(Part { ordinal:r.try_get("ordinal")?, identity:r.try_get("page_identity")?, checkpoint:parsed.and_then(Result::ok), invalid })
        }).collect()
}
pub(super) fn part_identities(
    manifest: Option<&MangaDexManifest>,
) -> Result<Vec<(u32, String)>, DirectError> {
    if let Some(manifest) = manifest {
        (0..manifest.summary().page_count)
            .map(|i| {
                Ok((
                    i + 1,
                    manifest
                        .page_identity(i as usize)
                        .map_err(|_| DirectError::Changed)?,
                ))
            })
            .collect()
    } else {
        Ok(vec![(0, hash(b"archive"))])
    }
}
async fn review_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    id: &str,
) -> Result<sqlx::sqlite::SqliteRow, DirectError> {
    sqlx::query("SELECT a.*,r.revision AS review_revision FROM direct_acquisitions a JOIN direct_review_revisions r ON r.acquisition_id=a.id WHERE a.id=? AND a.owner=?")
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(DirectError::NotFound)
}
async fn blocked_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &sqlx::sqlite::SqliteRow,
) -> Result<bool, DirectError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM import_operations WHERE id=? OR unit_id=? OR (destination_root=? AND destination_relative=?)) OR EXISTS(SELECT 1 FROM file_coverage WHERE unit_id=?)")
        .bind(row.try_get::<String,_>("import_id")?).bind(row.try_get::<String,_>("unit_id")?).bind(row.try_get::<String,_>("destination_root_id")?).bind(row.try_get::<String,_>("destination_relative")?).bind(row.try_get::<String,_>("unit_id")?).fetch_one(&mut **tx).await?)
}
fn idle_reason(
    row: &sqlx::sqlite::SqliteRow,
    blocked: bool,
) -> Result<Option<&'static str>, DirectError> {
    Ok(
        if blocked
            || row
                .try_get::<Option<String>, _>("source_relative")?
                .is_some()
        {
            Some("import_review_required")
        } else if row.try_get::<&str, _>("state")? != "needs_review" {
            Some("transfer_not_idle")
        } else {
            None
        },
    )
}
async fn action_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    id: &str,
    request: &TransferAction,
    prepared_evidence: Option<&str>,
) -> Result<sqlx::sqlite::SqliteRow, DirectError> {
    let row = review_row(tx, owner, id).await?;
    let current = evidence_in(tx, id).await?;
    // Writers compare the captured facts directly; hashing happens only during read preparation.
    let changed = if let Some(prepared) = prepared_evidence {
        current != prepared
    } else {
        hash(current.as_bytes()) != request.transfer_fingerprint
    };
    if row.try_get::<i64, _>("review_revision")? != request.revision || changed {
        return Err(DirectError::ReviewConflict);
    }
    if idle_reason(&row, blocked_in(tx, &row).await?)?.is_some() {
        return Err(DirectError::ReviewRequired);
    }
    Ok(row)
}
async fn fingerprint_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<String, DirectError> {
    Ok(hash(evidence_in(tx, id).await?.as_bytes()))
}
async fn evidence_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<String, DirectError> {
    Ok(sqlx::query_scalar("SELECT json_array(a.owner,a.link_handle,a.unit_id,a.download_root_id,a.destination_root_id,a.destination_relative,a.import_id,a.attempted,a.state,a.reason,(SELECT revision FROM direct_review_revisions WHERE acquisition_id=a.id),a.manifest_identity,a.expected_pages,a.root_identity,a.destination_identity,a.source_relative,(SELECT json_group_array(json_array(ordinal,page_identity,checkpoint)) FROM (SELECT ordinal,page_identity,checkpoint FROM direct_transfer_parts WHERE acquisition_id=a.id ORDER BY ordinal)),(SELECT path FROM library_roots WHERE id=a.download_root_id),(SELECT path FROM library_roots WHERE id=a.destination_root_id),(SELECT json_array(s.source_fingerprint,s.source_kind,(SELECT json_array(kind,base_url,enabled,options,secret_version,hex(secret_nonce),hex(secret_ciphertext)) FROM integrations WHERE id=s.integration_id)) FROM direct_selections s WHERE s.handle=a.link_handle)) FROM direct_acquisitions a WHERE a.id=?")
        .bind(id).fetch_one(&mut **tx).await?
    )
}
async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    action: &str,
    request: &TransferAction,
    replacement: Option<&str>,
) -> Result<(), DirectError> {
    sqlx::query("INSERT INTO direct_transfer_events(acquisition_id,action,revision,transfer_fingerprint,replacement_id) VALUES(?,?,?,?,?)")
        .bind(id).bind(action).bind(request.revision).bind(&request.transfer_fingerprint).bind(replacement).execute(&mut **tx).await?;
    Ok(())
}
pub(super) fn page_name(ordinal: u32) -> String {
    format!("page-{ordinal:06}.partial")
}
fn page_absent(job: &File, name: &str) -> Result<bool, DirectError> {
    match std::fs::symlink_metadata(format!("/proc/self/fd/{}/{name}", job.as_raw_fd())) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err(DirectError::LocalReview),
    }
}
fn check_page_names(job: &File, parts: &[Part]) -> Result<(), DirectError> {
    let actual = std::fs::read_dir(format!("/proc/self/fd/{}", job.as_raw_fd()))
        .map_err(|_| DirectError::LocalReview)?
        .map(|entry| {
            entry
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|_| DirectError::LocalReview)
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    let expected = std::iter::once("filepartial".to_owned())
        .chain(parts.iter().map(|p| page_name(p.ordinal)))
        .collect::<std::collections::BTreeSet<_>>();
    if !actual.is_subset(&expected)
        || !actual.contains("filepartial")
        || parts
            .iter()
            .any(|p| !p.fresh_page() && !actual.contains(&page_name(p.ordinal)))
    {
        return Err(DirectError::RestartRequired);
    }
    Ok(())
}
