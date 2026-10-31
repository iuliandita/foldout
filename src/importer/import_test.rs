use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

use tempfile::{TempDir, tempdir};
use uuid::Uuid;

use super::{
    finalize,
    journal::{
        ImportError, ImportMode, ImportPhase, ImportPolicy, ImportService, InternalImportRequest,
        SourceCleanup,
    },
};
use crate::{library::roots::Library, store::sqlite::SqliteStore};

const PDF: &[u8] = include_bytes!("../../tests/fixtures/single-page.pdf");
const CBZ: &[u8] = include_bytes!("../../tests/fixtures/natural-order.cbz");

struct Fixture {
    state: TempDir,
    source: TempDir,
    destination: TempDir,
    store: SqliteStore,
    request: InternalImportRequest,
    id: String,
}

impl Fixture {
    async fn new(policy: ImportPolicy) -> Self {
        let state = private_directory();
        let source = tempdir().unwrap();
        let destination = tempdir().unwrap();
        Self::at(state, source, destination, policy).await
    }

    async fn at(
        state: TempDir,
        source: TempDir,
        destination: TempDir,
        policy: ImportPolicy,
    ) -> Self {
        let store = SqliteStore::open(state.path()).await.unwrap();
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO publications (id, content_type, title, sort_title) VALUES ('pub', 'comic', 'Owned fixture', 'Owned fixture')").execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO editions (id, publication_id, language) VALUES ('edition', 'pub', 'en')",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO units (id, edition_id, label, kind) VALUES ('unit', 'edition', '1', 'issue')").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let library = Library::new(store.clone());
        let source_root = library
            .register_root("source", source.path())
            .await
            .unwrap()
            .id;
        let destination_root = library
            .register_root("destination", destination.path())
            .await
            .unwrap()
            .id;
        fs::write(source.path().join("owned.pdf"), PDF).unwrap();
        Self {
            state,
            source,
            destination,
            store,
            request: InternalImportRequest {
                source_root,
                source_relative: "owned.pdf".into(),
                destination_root,
                destination_relative: "imported.pdf".into(),
                unit_id: "unit".into(),
                policy,
            },
            id: Uuid::new_v4().to_string(),
        }
    }

    fn service(&self) -> ImportService {
        ImportService::new(self.store.clone())
    }

    async fn plan(&self) {
        assert_eq!(
            self.service()
                .plan(&self.id, self.request.clone(), ImportMode::Standalone)
                .await
                .unwrap()
                .phase,
            ImportPhase::Planned
        );
        assert!(!self.destination.path().join(".library-imports").exists());
    }

    async fn assert_done(&self) {
        let result = self.service().recover(&self.id).await.unwrap();
        assert_eq!(result.phase, ImportPhase::Done);
        assert_eq!(
            self.service()
                .recover(&self.id)
                .await
                .unwrap()
                .library_file_id,
            result.library_file_id
        );
        let source = self.source.path().join(&self.request.source_relative);
        if self.request.policy == ImportPolicy::Move {
            assert!(!source.exists());
            assert_eq!(result.effective_policy.as_deref(), Some("move"));
        } else {
            assert_eq!(fs::read(source).unwrap(), PDF);
        }
        assert_eq!(
            fs::read(
                self.destination
                    .path()
                    .join(&self.request.destination_relative)
            )
            .unwrap(),
            PDF
        );
        assert!(
            !self
                .destination
                .path()
                .join(".library-imports")
                .join(format!("{}.tmp", self.id))
                .exists()
        );
        let count: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM library_files), (SELECT count(*) FROM file_coverage)",
        )
        .fetch_one(self.store.reader())
        .await
        .unwrap();
        assert_eq!(count, (1, 1));
    }
}

#[tokio::test]
async fn recovery_requires_durable_catalog_evidence_before_cleanup() {
    for policy in [
        ImportPolicy::Copy,
        ImportPolicy::Move,
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        },
    ] {
        for phase in [ImportPhase::Cataloged, ImportPhase::CleanupPending] {
            for remove_coverage in [true, false] {
                let mut fixture = Fixture::new(policy.clone()).await;
                fixture.plan().await;
                let operation = loop {
                    let operation = fixture.service().step(&fixture.id).await.unwrap();
                    if operation.phase == phase {
                        break operation;
                    }
                };
                let file_id = operation.library_file_id.as_ref().unwrap();
                let coverage: (String, String, String) = sqlx::query_as(
                    "SELECT library_file_id, unit_id, evidence FROM file_coverage WHERE library_file_id = ?",
                )
                .bind(file_id)
                .fetch_one(fixture.store.reader())
                .await
                .unwrap();
                let signature: String =
                    sqlx::query_scalar("SELECT signature FROM library_files WHERE id = ?")
                        .bind(file_id)
                        .fetch_one(fixture.store.reader())
                        .await
                        .unwrap();
                let paths = [
                    fixture.source.path().join("owned.pdf"),
                    fixture.destination.path().join("imported.pdf"),
                    fixture
                        .destination
                        .path()
                        .join(".library-imports")
                        .join(format!("{}.tmp", fixture.id)),
                ];
                let identities = paths.each_ref().map(|path| {
                    assert_eq!(fs::read(path).unwrap(), PDF);
                    let metadata = fs::metadata(path).unwrap();
                    (
                        metadata.dev(),
                        metadata.ino(),
                        metadata.mode(),
                        metadata.len(),
                    )
                });
                let mut tx = fixture.store.begin_write().await.unwrap();
                let changed = if remove_coverage {
                    sqlx::query(
                        "DELETE FROM file_coverage WHERE library_file_id = ? AND unit_id = ?",
                    )
                    .bind(&coverage.0)
                    .bind(&coverage.1)
                    .execute(&mut *tx)
                    .await
                    .unwrap()
                } else {
                    sqlx::query("UPDATE library_files SET signature = ? WHERE id = ?")
                        .bind(format!("{signature}-corrupt"))
                        .bind(file_id)
                        .execute(&mut *tx)
                        .await
                        .unwrap()
                };
                assert_eq!(changed.rows_affected(), 1);
                tx.commit().await.unwrap();
                fixture.store.close().await;
                fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();

                let error = fixture.service().recover(&fixture.id).await.unwrap_err();
                assert!(matches!(error, ImportError::InvalidJournal));
                let blocked = fixture.service().get(&fixture.id).await.unwrap();
                assert_eq!(blocked.phase, phase);
                assert_eq!(blocked.library_file_id, operation.library_file_id);
                assert_eq!(
                    blocked.last_error.as_deref(),
                    Some(error.to_string().as_str())
                );
                for (path, identity) in paths.iter().zip(identities) {
                    assert_eq!(fs::read(path).unwrap(), PDF);
                    let metadata = fs::metadata(path).unwrap();
                    assert_eq!(
                        (
                            metadata.dev(),
                            metadata.ino(),
                            metadata.mode(),
                            metadata.len()
                        ),
                        identity
                    );
                }

                let mut tx = fixture.store.begin_write().await.unwrap();
                let restored = if remove_coverage {
                    sqlx::query("INSERT INTO file_coverage (library_file_id, unit_id, evidence) VALUES (?, ?, ?)")
                        .bind(&coverage.0)
                        .bind(&coverage.1)
                        .bind(&coverage.2)
                        .execute(&mut *tx)
                        .await
                        .unwrap()
                } else {
                    sqlx::query("UPDATE library_files SET signature = ? WHERE id = ?")
                        .bind(&signature)
                        .bind(file_id)
                        .execute(&mut *tx)
                        .await
                        .unwrap()
                };
                assert_eq!(restored.rows_affected(), 1);
                tx.commit().await.unwrap();
                fixture.store.close().await;
                fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();
                fixture.assert_done().await;
                let done = fixture.service().get(&fixture.id).await.unwrap();
                assert_eq!(done.library_file_id, operation.library_file_id);
                assert!(done.last_error.is_none());
                let restored: (String, String, String, String) = sqlx::query_as(
                    "SELECT c.library_file_id, c.unit_id, c.evidence, f.signature FROM file_coverage c JOIN library_files f ON f.id = c.library_file_id",
                )
                .fetch_one(fixture.store.reader())
                .await
                .unwrap();
                assert_eq!(restored, (coverage.0, coverage.1, coverage.2, signature));
            }
        }
    }
}

#[tokio::test]
async fn restart_after_every_committed_phase_preserves_source_and_one_catalog_file() {
    for policy in [
        ImportPolicy::Copy,
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        },
    ] {
        for phase_count in 0..=6 {
            let mut fixture = Fixture::new(policy.clone()).await;
            fixture.plan().await;
            let original = fs::metadata(fixture.source.path().join("owned.pdf")).unwrap();
            for _ in 0..phase_count {
                fixture.service().step(&fixture.id).await.unwrap();
            }
            let mut tx = fixture.store.begin_write().await.unwrap();
            sqlx::query("UPDATE import_operations SET identity_json = json_remove(identity_json, '$.source_mtime_ns', '$.source_parent_identity', '$.mode') WHERE id = ?")
                .bind(&fixture.id).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            fixture.store.close().await;
            fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();
            fixture.assert_done().await;
            let source = fs::metadata(fixture.source.path().join("owned.pdf")).unwrap();
            let dest = fs::metadata(fixture.destination.path().join("imported.pdf")).unwrap();
            assert_eq!(
                (source.ino(), source.mode()),
                (original.ino(), original.mode())
            );
            assert_eq!(
                source.ino() == dest.ino(),
                matches!(policy, ImportPolicy::Hardlink { .. })
            );
            assert_eq!(
                fs::metadata(fixture.destination.path().join(".library-imports"))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
}

#[tokio::test]
async fn duplicate_plan_is_idempotent_and_conflicting_id_or_destination_is_rejected() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    fixture.plan().await;
    let mut changed = fixture.request.clone();
    changed.policy = ImportPolicy::Hardlink {
        fallback_to_copy: true,
    };
    assert!(matches!(
        fixture
            .service()
            .plan(&fixture.id, changed, ImportMode::Standalone)
            .await,
        Err(ImportError::Conflict)
    ));
    assert!(matches!(
        fixture
            .service()
            .plan(
                &Uuid::new_v4().to_string(),
                fixture.request.clone(),
                ImportMode::Standalone
            )
            .await,
        Err(ImportError::Conflict)
    ));
    fixture.assert_done().await;
}

#[tokio::test]
async fn collision_before_plan_or_after_intent_never_overwrites_even_identical_bytes() {
    for bytes in [b"unrelated".as_slice(), PDF] {
        let fixture = Fixture::new(ImportPolicy::Copy).await;
        let path = fixture.destination.path().join("imported.pdf");
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            fixture
                .service()
                .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
                .await,
            Err(ImportError::Conflict)
        ));
        fs::remove_file(&path).unwrap();
        fixture.plan().await;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            fixture.service().recover(&fixture.id).await,
            Err(ImportError::Conflict)
        ));
        assert_eq!(fs::read(path).unwrap(), bytes);
        let op = fixture.service().get(&fixture.id).await.unwrap();
        assert_eq!(op.phase, ImportPhase::Planned);
        assert!(op.last_error.is_some());
        assert_eq!(
            fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
            PDF
        );
    }
}

#[tokio::test]
async fn source_changed_after_intent_stops_before_finalization() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    fs::write(fixture.source.path().join("owned.pdf"), b"%PDF-1.4 altered").unwrap();
    assert!(matches!(
        fixture.service().recover(&fixture.id).await,
        Err(ImportError::Changed)
    ));
    assert!(!fixture.destination.path().join("imported.pdf").exists());
}

#[tokio::test]
async fn replaced_source_inode_with_same_bytes_is_not_the_planned_source() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    fs::rename(
        fixture.source.path().join("owned.pdf"),
        fixture.source.path().join("preserved.pdf"),
    )
    .unwrap();
    fs::write(fixture.source.path().join("owned.pdf"), PDF).unwrap();
    assert!(matches!(
        fixture.service().recover(&fixture.id).await,
        Err(ImportError::Changed)
    ));
}

#[tokio::test]
async fn recovers_effects_before_their_database_phase_commits() {
    for policy in [
        ImportPolicy::Copy,
        ImportPolicy::Move,
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        },
    ] {
        let fixture = Fixture::new(policy.clone()).await;
        fixture.plan().await;
        let op = fixture.service().get(&fixture.id).await.unwrap();
        finalize::stage(&op).unwrap();
        fixture.service().step(&fixture.id).await.unwrap();
        fixture.service().step(&fixture.id).await.unwrap();
        let verified = fixture.service().get(&fixture.id).await.unwrap();
        assert_eq!(verified.phase, ImportPhase::Verified);
        finalize::destination(&verified)
            .unwrap()
            .publish(&verified)
            .unwrap();
        fixture.service().step(&fixture.id).await.unwrap();
        fixture.service().step(&fixture.id).await.unwrap();
        fixture.service().step(&fixture.id).await.unwrap();
        let cleanup = fixture.service().get(&fixture.id).await.unwrap();
        assert_eq!(cleanup.phase, ImportPhase::CleanupPending);
        finalize::destination(&cleanup)
            .unwrap()
            .cleanup(&cleanup)
            .unwrap();
        if policy == ImportPolicy::Move {
            assert!(!fixture.service().cleanup_source(&cleanup).await.unwrap());
            let cleanup = fixture.service().get(&fixture.id).await.unwrap();
            assert!(fixture.service().cleanup_source(&cleanup).await.unwrap());
            assert!(!fixture.source.path().join("owned.pdf").exists());
        }
        fixture.assert_done().await;
    }
}

#[tokio::test]
async fn interrupted_partial_copy_is_rebuilt_only_inside_private_staging() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    let op = fixture.service().get(&fixture.id).await.unwrap();
    let _dest = finalize::destination(&op).unwrap();
    let path = fixture
        .destination
        .path()
        .join(".library-imports")
        .join(format!("{}.tmp", fixture.id));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(b"%PDF-").unwrap();
    drop(file);
    fixture.assert_done().await;
}

#[tokio::test]
async fn changed_stage_or_final_file_is_never_accepted() {
    for (policy, phase_count, changed_inode) in [ImportPolicy::Copy, ImportPolicy::Move]
        .into_iter()
        .flat_map(|policy| {
            [(1, true), (3, true), (5, true), (6, true), (5, false)]
                .map(|(phase, changed_inode)| (policy.clone(), phase, changed_inode))
        })
    {
        let fixture = Fixture::new(policy).await;
        fixture.plan().await;
        for _ in 0..phase_count {
            fixture.service().step(&fixture.id).await.unwrap();
        }
        let path = if phase_count == 1 {
            fixture
                .destination
                .path()
                .join(".library-imports")
                .join(format!("{}.tmp", fixture.id))
        } else {
            fixture.destination.path().join("imported.pdf")
        };
        if changed_inode {
            fs::rename(&path, path.with_extension("preserved")).unwrap();
            fs::write(&path, PDF).unwrap();
        } else {
            let mut bytes = PDF.to_vec();
            bytes[0] ^= 1;
            fs::write(&path, bytes).unwrap();
        }
        assert!(matches!(
            fixture.service().recover(&fixture.id).await,
            Err(ImportError::Changed)
        ));
        if phase_count < 6 {
            assert_eq!(
                fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
                PDF
            );
        }
    }
}

#[tokio::test]
async fn move_refuses_source_changes_after_catalog_commit() {
    for change in [
        "inode", "content", "size", "mtime", "symlink", "parent", "legacy",
    ] {
        let mut fixture = Fixture::new(ImportPolicy::Move).await;
        fs::create_dir(fixture.source.path().join("nested")).unwrap();
        fs::rename(
            fixture.source.path().join("owned.pdf"),
            fixture.source.path().join("nested/owned.pdf"),
        )
        .unwrap();
        fixture.request.source_relative = "nested/owned.pdf".into();
        fixture.plan().await;
        for _ in 0..5 {
            fixture.service().step(&fixture.id).await.unwrap();
        }
        let source = fixture.source.path().join("nested/owned.pdf");
        match change {
            "inode" => {
                fs::rename(&source, source.with_extension("saved")).unwrap();
                fs::write(&source, PDF).unwrap();
            }
            "content" | "size" => {
                let modified = fs::metadata(&source).unwrap().modified().unwrap();
                let mut bytes = PDF.to_vec();
                if change == "size" {
                    bytes.push(b' ');
                } else {
                    bytes[0] ^= 1;
                }
                fs::write(&source, bytes).unwrap();
                if change == "content" {
                    fs::File::open(&source)
                        .unwrap()
                        .set_modified(modified)
                        .unwrap();
                }
            }
            "mtime" => {
                let file = fs::File::open(&source).unwrap();
                let changed = file.metadata().unwrap().modified().unwrap()
                    + std::time::Duration::from_secs(1);
                file.set_modified(changed).unwrap();
            }
            "symlink" => {
                let saved = source.with_extension("saved");
                fs::rename(&source, &saved).unwrap();
                symlink(saved, &source).unwrap();
            }
            "parent" => {
                fs::rename(
                    fixture.source.path().join("nested"),
                    fixture.source.path().join("saved"),
                )
                .unwrap();
                fs::create_dir(fixture.source.path().join("nested")).unwrap();
                fs::hard_link(fixture.source.path().join("saved/owned.pdf"), &source).unwrap();
            }
            "legacy" => {
                let mut tx = fixture.store.begin_write().await.unwrap();
                sqlx::query("UPDATE import_operations SET identity_json = json_remove(identity_json, '$.source_mtime_ns', '$.source_parent_identity') WHERE id = ?")
                    .bind(&fixture.id).execute(&mut *tx).await.unwrap();
                tx.commit().await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = fs::read(&source).unwrap();
        fixture.store.close().await;
        fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();
        let error = fixture.service().recover(&fixture.id).await.unwrap_err();
        if change == "legacy" {
            assert!(matches!(error, ImportError::InvalidJournal));
        } else if change == "symlink" {
            assert!(matches!(error, ImportError::Io(_)));
        } else {
            assert!(matches!(error, ImportError::Changed), "{change}: {error}");
        }
        assert_eq!(fs::read(&source).unwrap(), before);
        assert_eq!(
            fs::read(fixture.destination.path().join("imported.pdf")).unwrap(),
            PDF
        );
        let operation = fixture.service().get(&fixture.id).await.unwrap();
        assert_eq!(operation.phase, ImportPhase::CleanupPending);
        assert!(operation.last_error.is_some());
        assert!(operation.library_file_id.is_some());
    }
}

#[tokio::test]
async fn move_unlink_permission_failure_retains_source_and_retries() {
    for captured in [false, true] {
        let fixture = pending_move().await;
        let (directory, source) = if captured {
            fixture.service().step(&fixture.id).await.unwrap();
            let directory = fixture
                .source
                .path()
                .join(format!(".library-move-{}", fixture.id));
            (directory.clone(), directory.join("source"))
        } else {
            (
                fixture.source.path().to_path_buf(),
                fixture.source.path().join("owned.pdf"),
            )
        };
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
        let result = fixture.service().recover(&fixture.id).await;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(result, Err(ImportError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert_eq!(fs::read(&source).unwrap(), PDF);
        assert_eq!(
            fs::read(fixture.destination.path().join("imported.pdf")).unwrap(),
            PDF
        );
        let operation = fixture.service().get(&fixture.id).await.unwrap();
        assert_eq!(operation.phase, ImportPhase::CleanupPending);
        assert!(operation.last_error.is_some());
        fixture.assert_done().await;
    }
}

#[tokio::test]
async fn move_copy_disk_full_preserves_source_and_rebuilds_partial_stage() {
    use std::io::Write;

    let fixture = Fixture::new(ImportPolicy::Move).await;
    fixture.plan().await;
    let operation = fixture.service().get(&fixture.id).await.unwrap();
    let result = finalize::stage_with_copy(&operation, |_, target| {
        target.write_all(b"%PDF-")?;
        Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
    });
    assert!(
        matches!(result, Err(ImportError::Io(error)) if error.raw_os_error() == Some(libc::ENOSPC))
    );
    assert_eq!(
        fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
        PDF
    );
    assert!(!fixture.destination.path().join("imported.pdf").exists());
    assert_eq!(
        fixture.service().get(&fixture.id).await.unwrap().phase,
        ImportPhase::Planned
    );
    assert_eq!(
        fs::read(
            fixture
                .destination
                .path()
                .join(".library-imports")
                .join(format!("{}.tmp", fixture.id))
        )
        .unwrap(),
        b"%PDF-"
    );
    fixture.assert_done().await;
}

#[tokio::test]
async fn torrent_preserving_policy_rejects_move_before_any_journal_or_staging_write() {
    for policy in [
        ImportPolicy::Move,
        ImportPolicy::Copy,
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        },
    ] {
        let fixture = Fixture::new(policy.clone()).await;
        let result = fixture
            .service()
            .plan(
                &fixture.id,
                fixture.request.clone(),
                ImportMode::TorrentPreserving,
            )
            .await;
        if policy == ImportPolicy::Move {
            assert!(matches!(result, Err(ImportError::TorrentPreserving)));
            assert!(matches!(
                fixture.service().get(&fixture.id).await,
                Err(ImportError::NotFound)
            ));
            assert!(!fixture.destination.path().join(".library-imports").exists());
            assert_eq!(
                fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
                PDF
            );
        } else {
            let operation = result.unwrap();
            assert_eq!(operation.identity.mode, ImportMode::TorrentPreserving);
            fixture.assert_done().await;
        }
    }
}

#[tokio::test]
async fn no_replace_at_publish_rejects_a_late_destination_collision() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    fixture.service().step(&fixture.id).await.unwrap();
    fixture.service().step(&fixture.id).await.unwrap();
    fs::write(fixture.destination.path().join("imported.pdf"), PDF).unwrap();
    assert!(matches!(
        fixture.service().recover(&fixture.id).await,
        Err(ImportError::Conflict)
    ));
    assert_eq!(
        fixture.service().get(&fixture.id).await.unwrap().phase,
        ImportPhase::Verified
    );
}

#[tokio::test]
async fn rejects_traversal_symlinks_and_replaced_destination_parent() {
    let mut fixture = Fixture::new(ImportPolicy::Copy).await;
    for path in [
        "../escape.pdf",
        "/absolute.pdf",
        "a/../x.pdf",
        "a//x.pdf",
        ".library-imports/x.pdf",
        "./x.pdf",
    ] {
        let mut request = fixture.request.clone();
        request.destination_relative = path.into();
        assert!(matches!(
            fixture
                .service()
                .plan(&fixture.id, request, ImportMode::Standalone)
                .await,
            Err(ImportError::UnsafePath)
        ));
    }
    let outside = tempdir().unwrap();
    symlink(outside.path(), fixture.destination.path().join("link")).unwrap();
    fixture.request.destination_relative = "link/imported.pdf".into();
    assert!(
        fixture
            .service()
            .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
            .await
            .is_err()
    );
    fs::create_dir(fixture.destination.path().join("nested")).unwrap();
    fixture.request.destination_relative = "nested/imported.pdf".into();
    fixture.plan().await;
    fs::rename(
        fixture.destination.path().join("nested"),
        fixture.destination.path().join("saved"),
    )
    .unwrap();
    symlink(outside.path(), fixture.destination.path().join("nested")).unwrap();
    assert!(fixture.service().recover(&fixture.id).await.is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn unsafe_stage_directory_or_stage_symlink_cannot_touch_target() {
    for directory_link in [false, true] {
        let fixture = Fixture::new(ImportPolicy::Copy).await;
        fixture.plan().await;
        let outside = tempdir().unwrap();
        let protected = outside.path().join("protected.pdf");
        fs::write(&protected, b"protected").unwrap();
        let directory = fixture.destination.path().join(".library-imports");
        if directory_link {
            symlink(outside.path(), &directory).unwrap();
        } else {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap();
            symlink(&protected, directory.join(format!("{}.tmp", fixture.id))).unwrap();
        }
        assert!(fixture.service().recover(&fixture.id).await.is_err());
        assert_eq!(fs::read(protected).unwrap(), b"protected");
    }
}

#[tokio::test]
async fn owned_cbz_is_decoded_and_invalid_pdf_is_not_cataloged() {
    let mut fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.request.source_relative = "owned.cbz".into();
    fixture.request.destination_relative = "imported.cbz".into();
    fs::write(fixture.source.path().join("owned.cbz"), CBZ).unwrap();
    fixture.plan().await;
    let result = fixture.service().recover(&fixture.id).await.unwrap();
    assert_eq!(result.validation.as_deref(), Some("archive_pages_verified"));
    assert_eq!(
        fs::read(fixture.source.path().join("owned.cbz")).unwrap(),
        CBZ
    );
    assert_eq!(
        fs::read(fixture.destination.path().join("imported.cbz")).unwrap(),
        CBZ
    );
    let invalid = Fixture::new(ImportPolicy::Copy).await;
    fs::write(invalid.source.path().join("owned.pdf"), b"not a PDF").unwrap();
    invalid.plan().await;
    assert!(matches!(
        invalid.service().recover(&invalid.id).await,
        Err(ImportError::InvalidFormat)
    ));
    assert_eq!(
        invalid.service().get(&invalid.id).await.unwrap().phase,
        ImportPhase::Staged
    );
    assert!(!invalid.destination.path().join("imported.pdf").exists());
}

#[tokio::test]
async fn pdf_manifest_rejects_fake_header_and_accepts_owned_document() {
    for bytes in [b"%PDF-1.4\nnot a document\n%%EOF\n".as_slice(), PDF] {
        let fixture = Fixture::new(ImportPolicy::Copy).await;
        fs::write(fixture.source.path().join("owned.pdf"), bytes).unwrap();
        fixture.plan().await;
        fixture.service().step(&fixture.id).await.unwrap();
        let staged = fixture.service().get(&fixture.id).await.unwrap();
        let stage_identity = staged.staged.clone();
        let result = fixture.service().step(&fixture.id).await;
        if bytes == PDF {
            let verified = result.unwrap();
            assert_eq!(verified.phase, ImportPhase::Verified);
            assert_eq!(
                verified.validation.as_deref(),
                Some("pdf_manifest_verified")
            );
            assert_eq!(verified.staged, stage_identity);
            fixture.assert_done().await;
        } else {
            assert!(matches!(result, Err(ImportError::InvalidFormat)));
            let failed = fixture.service().get(&fixture.id).await.unwrap();
            assert_eq!(failed.phase, ImportPhase::Staged);
            assert!(failed.validation.is_none());
            assert!(failed.last_error.is_some());
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM library_files")
                .fetch_one(fixture.store.reader())
                .await
                .unwrap();
            assert_eq!(count, 0);
            assert!(!fixture.destination.path().join("imported.pdf").exists());
        }
        assert_eq!(
            fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
            bytes
        );
    }
}

#[tokio::test]
async fn legacy_signature_only_pdf_is_revalidated_before_publication() {
    for bytes in [b"%PDF-1.4\nnot a document\n%%EOF\n".as_slice(), PDF] {
        let fixture = Fixture::new(ImportPolicy::Copy).await;
        fs::write(fixture.source.path().join("owned.pdf"), bytes).unwrap();
        fixture.plan().await;
        let staged = fixture.service().step(&fixture.id).await.unwrap();
        fixture
            .service()
            .advance(
                &staged,
                ImportPhase::Verified,
                None,
                None,
                Some("pdf_signature_only"),
            )
            .await
            .unwrap();
        let result = fixture.service().step(&fixture.id).await;
        if bytes == PDF {
            let finalized = result.unwrap();
            assert_eq!(finalized.phase, ImportPhase::Finalized);
            assert_eq!(
                finalized.validation.as_deref(),
                Some("pdf_manifest_verified")
            );
            fixture.assert_done().await;
        } else {
            assert!(matches!(result, Err(ImportError::InvalidFormat)));
            assert_eq!(
                fixture.service().get(&fixture.id).await.unwrap().phase,
                ImportPhase::Verified
            );
            assert!(!fixture.destination.path().join("imported.pdf").exists());
        }
    }
}

#[tokio::test]
#[ignore = "requires IMPORT_TEST_CROSS_FS_ROOT on an explicitly approved scratch filesystem"]
async fn cross_filesystem_fallback_is_explicit_when_scratch_root_is_configured() {
    let path = std::env::var_os("IMPORT_TEST_CROSS_FS_ROOT")
        .expect("set IMPORT_TEST_CROSS_FS_ROOT to an approved scratch filesystem");
    for fallback in [false, true] {
        let fixture = Fixture::at(
            private_directory(),
            tempfile::tempdir_in(Path::new(&path)).unwrap(),
            tempdir().unwrap(),
            ImportPolicy::Hardlink {
                fallback_to_copy: fallback,
            },
        )
        .await;
        assert_ne!(
            fs::metadata(fixture.source.path()).unwrap().dev(),
            fs::metadata(fixture.destination.path()).unwrap().dev(),
            "scratch must use another filesystem"
        );
        fixture.plan().await;
        if fallback {
            fixture.assert_done().await;
            assert_eq!(
                fixture
                    .service()
                    .get(&fixture.id)
                    .await
                    .unwrap()
                    .effective_policy
                    .as_deref(),
                Some("copy")
            );
        } else {
            assert!(matches!(
                fixture.service().recover(&fixture.id).await,
                Err(ImportError::CrossFilesystem)
            ));
            assert_eq!(
                fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
                PDF
            );
        }
    }
}

#[tokio::test]
async fn flock_blocks_other_service_instances_and_releases_on_drop() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    let op = fixture.service().get(&fixture.id).await.unwrap();
    let lock = finalize::lock(&op).unwrap();
    // dup models a descriptor inherited by a decoder before its exec closes it.
    let inherited = lock.0.try_clone().unwrap();
    assert!(matches!(
        fixture.service().step(&fixture.id).await,
        Err(ImportError::Busy)
    ));
    drop(lock);
    fixture.assert_done().await;
    drop(inherited);
}

#[tokio::test]
async fn catalog_and_coverage_rollback_together_before_cleanup() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    for _ in 0..3 {
        fixture.service().step(&fixture.id).await.unwrap();
    }
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("CREATE TRIGGER reject_import_coverage BEFORE INSERT ON file_coverage BEGIN SELECT RAISE(ABORT, 'injected catalog failure'); END")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(fixture.service().step(&fixture.id).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM library_files")
        .fetch_one(fixture.store.reader())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        fixture.service().get(&fixture.id).await.unwrap().phase,
        ImportPhase::Finalized
    );
    assert!(
        fixture
            .destination
            .path()
            .join(".library-imports")
            .join(format!("{}.tmp", fixture.id))
            .exists()
    );
    assert_eq!(
        fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
        PDF
    );
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("DROP TRIGGER reject_import_coverage")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    fixture.assert_done().await;
}

#[tokio::test]
async fn source_symlink_before_or_after_plan_is_never_followed() {
    for after_plan in [false, true] {
        let fixture = Fixture::new(ImportPolicy::Hardlink {
            fallback_to_copy: true,
        })
        .await;
        if after_plan {
            fixture.plan().await;
        }
        let source = fixture.source.path().join("owned.pdf");
        let saved = fixture.source.path().join("saved.pdf");
        fs::rename(&source, &saved).unwrap();
        symlink(&saved, &source).unwrap();
        let result = if after_plan {
            fixture.service().recover(&fixture.id).await
        } else {
            fixture
                .service()
                .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
                .await
        };
        assert!(result.is_err());
        assert_eq!(fs::read(saved).unwrap(), PDF);
        assert!(!fixture.destination.path().join("imported.pdf").exists());
    }
}

#[tokio::test]
async fn cleanup_failure_keeps_catalog_and_source_and_remains_retryable() {
    let fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.plan().await;
    for _ in 0..5 {
        fixture.service().step(&fixture.id).await.unwrap();
    }
    let stage = fixture
        .destination
        .path()
        .join(".library-imports")
        .join(format!("{}.tmp", fixture.id));
    let saved = stage.with_extension("preserved");
    fs::rename(&stage, &saved).unwrap();
    symlink(&saved, &stage).unwrap();
    assert!(fixture.service().recover(&fixture.id).await.is_err());
    let operation = fixture.service().get(&fixture.id).await.unwrap();
    assert_eq!(operation.phase, ImportPhase::CleanupPending);
    assert!(operation.library_file_id.is_some());
    assert!(operation.last_error.is_some());
    assert_eq!(
        fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
        PDF
    );
    fs::remove_file(&stage).unwrap();
    fs::rename(saved, stage).unwrap();
    fixture.assert_done().await;
}

#[tokio::test]
#[ignore = "requires IMPORT_TEST_CBR_FILE pointing to explicitly authorized local media"]
async fn authorized_cbr_copy_import_preserves_external_source() {
    use std::{fs::File, os::unix::fs::OpenOptionsExt};

    let path = std::env::var_os("IMPORT_TEST_CBR_FILE")
        .expect("set IMPORT_TEST_CBR_FILE to explicitly authorized local media");
    let mut original = File::open(path).unwrap();
    let identity = finalize::Identity::of(&original).unwrap();
    let before = finalize::hash(&mut original).unwrap();
    let mut fixture = Fixture::new(ImportPolicy::Copy).await;
    fixture.request.source_relative = "fixture.cbr".into();
    fixture.request.destination_relative = "imported.cbr".into();
    let mut protected_copy = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.source.path().join("fixture.cbr"))
        .unwrap();
    std::io::Seek::rewind(&mut original).unwrap();
    std::io::copy(&mut original, &mut protected_copy).unwrap();
    protected_copy.sync_all().unwrap();
    assert_eq!(finalize::hash(&mut protected_copy).unwrap().0, before.0);
    fixture.plan().await;
    let done = fixture.service().recover(&fixture.id).await.unwrap();
    assert_eq!(done.phase, ImportPhase::Done);
    assert_eq!(done.validation.as_deref(), Some("archive_pages_verified"));
    assert_eq!(done.effective_policy.as_deref(), Some("copy"));
    let mut destination = File::open(fixture.destination.path().join("imported.cbr")).unwrap();
    assert_eq!(finalize::hash(&mut destination).unwrap().0, before.0);
    assert_eq!(finalize::hash(&mut protected_copy).unwrap().0, before.0);
    assert_eq!(finalize::Identity::of(&original).unwrap(), identity);
    assert_eq!(finalize::hash(&mut original).unwrap(), before);
    let count: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM library_files), (SELECT count(*) FROM file_coverage)",
    )
    .fetch_one(fixture.store.reader())
    .await
    .unwrap();
    assert_eq!(count, (1, 1));
    eprintln!(
        "CBR copy import completed; source and destination SHA-256 match ({} bytes)",
        before.1
    );
}

async fn pending_move() -> Fixture {
    let fixture = Fixture::new(ImportPolicy::Move).await;
    fixture.plan().await;
    for _ in 0..5 {
        fixture.service().step(&fixture.id).await.unwrap();
    }
    fixture
}

#[tokio::test]
async fn move_safety_catalog_guard_covers_both_keys_at_plan_replay_and_cleanup() {
    for boundary in ["plan", "replay", "cleanup"] {
        for by_path in [false, true] {
            let fixture = if boundary == "cleanup" {
                pending_move().await
            } else {
                Fixture::new(ImportPolicy::Move).await
            };
            if boundary == "replay" {
                fixture.plan().await;
            }
            let source = fixture.source.path().join("owned.pdf");
            let mut tx = fixture.store.begin_write().await.unwrap();
            sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path) VALUES('protected',?,'pdf','cataloged',1,?,?)")
                .bind(if by_path { source.to_str().unwrap().to_string() } else { fixture.source.path().join("other.pdf").to_str().unwrap().to_string() })
                .bind(if by_path { None } else { Some(&fixture.request.source_root) })
                .bind(if by_path { None } else { Some("owned.pdf") })
                .execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            let result = if boundary == "cleanup" {
                fixture.service().recover(&fixture.id).await
            } else {
                fixture
                    .service()
                    .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
                    .await
            };
            assert!(
                matches!(result, Err(ImportError::ProtectedSource)),
                "{boundary}, path={by_path}: {result:?}"
            );
            assert_eq!(fs::read(&source).unwrap(), PDF);
            assert!(
                !fixture
                    .source
                    .path()
                    .join(format!(".library-move-{}", fixture.id))
                    .exists()
            );
        }
    }
}

#[tokio::test]
async fn move_safety_disallows_equal_and_nested_canonical_roots_at_both_boundaries() {
    for relationship in ["equal", "destination_inside", "source_inside"] {
        for cleanup in [false, true] {
            let mut fixture = if cleanup {
                pending_move().await
            } else {
                Fixture::new(ImportPolicy::Move).await
            };
            let (source, destination) = match relationship {
                "equal" => (
                    fixture.source.path().to_path_buf(),
                    fixture.source.path().to_path_buf(),
                ),
                "destination_inside" => {
                    let child = fixture.source.path().join("child");
                    fs::create_dir(&child).unwrap();
                    (fixture.source.path().to_path_buf(), child)
                }
                "source_inside" => {
                    let child = fixture.destination.path().join("child");
                    fs::create_dir(&child).unwrap();
                    fs::write(child.join("owned.pdf"), PDF).unwrap();
                    (child, fixture.destination.path().to_path_buf())
                }
                _ => unreachable!(),
            };
            let mut tx = fixture.store.begin_write().await.unwrap();
            sqlx::query("UPDATE library_roots SET path=? WHERE id=?")
                .bind(fixture.destination.path().join("unused").to_str().unwrap())
                .bind(&fixture.request.destination_root)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE library_roots SET path=? WHERE id=?")
                .bind(source.to_str().unwrap())
                .bind(&fixture.request.source_root)
                .execute(&mut *tx)
                .await
                .unwrap();
            if relationship == "equal" {
                fixture.request.destination_root = fixture.request.source_root.clone();
            } else {
                sqlx::query("UPDATE library_roots SET path=? WHERE id=?")
                    .bind(destination.to_str().unwrap())
                    .bind(&fixture.request.destination_root)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
            if cleanup {
                let mut operation = fixture.service().get(&fixture.id).await.unwrap();
                operation.request = fixture.request.clone();
                operation.identity.source_root = source;
                operation.identity.destination_root = destination;
                let result = fixture.service().cleanup_source(&operation).await;
                assert!(
                    matches!(result, Err(ImportError::ProtectedSource)),
                    "{relationship}: {result:?}"
                );
            } else {
                let result = fixture
                    .service()
                    .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
                    .await;
                assert!(
                    matches!(result, Err(ImportError::ProtectedSource)),
                    "{relationship}: {result:?}"
                );
            }
            assert_eq!(
                fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
                PDF
            );
        }
    }
}

#[tokio::test]
async fn move_refuses_swapped_source_roots_and_canonical_aliases() {
    let fixture = pending_move().await;
    let holding = tempdir().unwrap();
    let original = holding.path().join("original");
    fs::rename(fixture.source.path(), &original).unwrap();
    fs::create_dir(fixture.source.path()).unwrap();
    fs::write(fixture.source.path().join("owned.pdf"), PDF).unwrap();
    assert!(matches!(
        fixture
            .service()
            .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
            .await,
        Err(ImportError::Changed)
    ));
    let result = fixture.service().recover(&fixture.id).await;
    assert!(matches!(result, Err(ImportError::Changed)));
    assert_eq!(
        fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
        PDF
    );
    fs::remove_dir_all(fixture.source.path()).unwrap();
    symlink(&original, fixture.source.path()).unwrap();
    let result = fixture
        .service()
        .plan(
            &Uuid::new_v4().to_string(),
            fixture.request.clone(),
            ImportMode::Standalone,
        )
        .await;
    assert!(matches!(result, Err(ImportError::UnsafePath)));
    fs::remove_file(fixture.source.path()).unwrap();
    fs::rename(original, fixture.source.path()).unwrap();
    fixture.assert_done().await;
}

#[tokio::test]
async fn move_safety_rename_race_restores_replacement_without_unlinking_either_file() {
    let fixture = pending_move().await;
    let operation = fixture.service().get(&fixture.id).await.unwrap();
    let tombstone = finalize::SourceTombstone::open(&operation).unwrap();
    let source = fixture.source.path().join("owned.pdf");
    let saved = fixture.source.path().join("saved.pdf");
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    let result = tombstone.quarantine_with(&operation, || {
        fs::rename(&source, &saved).unwrap();
        fs::write(&source, PDF).unwrap();
        fs::File::open(&source)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    });
    assert!(matches!(result, Err(ImportError::Changed)));
    assert_eq!(fs::read(&source).unwrap(), PDF);
    assert_eq!(fs::read(&saved).unwrap(), PDF);
    assert!(
        !fixture
            .source
            .path()
            .join(format!(".library-move-{}/source", fixture.id))
            .exists()
    );
    assert_eq!(
        fixture
            .service()
            .get(&fixture.id)
            .await
            .unwrap()
            .source_cleanup,
        SourceCleanup::Pending
    );
}

#[tokio::test]
async fn move_tombstone_recovery_undoes_uncommitted_and_finishes_verified_captures() {
    for verified in [false, true] {
        let mut fixture = pending_move().await;
        let operation = fixture.service().get(&fixture.id).await.unwrap();
        let tombstone = finalize::SourceTombstone::open(&operation).unwrap();
        if verified {
            fixture.service().step(&fixture.id).await.unwrap();
        } else {
            assert!(matches!(
                tombstone.quarantine(&operation).unwrap(),
                finalize::QuarantineResult::Verified
            ));
            assert!(matches!(
                tombstone.finish(&operation),
                Err(ImportError::InvalidJournal)
            ));
        }
        let captured = fixture
            .source
            .path()
            .join(format!(".library-move-{}/source", fixture.id));
        assert_eq!(fs::read(&captured).unwrap(), PDF);
        assert!(!fixture.source.path().join("owned.pdf").exists());
        fixture.store.close().await;
        fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();
        if !verified {
            let restored = fixture.service().step(&fixture.id).await.unwrap();
            assert_eq!(restored.source_cleanup, SourceCleanup::Pending);
            assert_eq!(restored.phase, ImportPhase::CleanupPending);
            assert_eq!(
                fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
                PDF
            );
            assert!(!captured.parent().unwrap().exists());
        }
        fixture.assert_done().await;
        assert!(!captured.parent().unwrap().exists());
    }
}

#[tokio::test]
async fn move_verified_tombstone_change_is_restored_and_new_original_names_are_preserved() {
    for changed_tombstone in [false, true] {
        let fixture = pending_move().await;
        fixture.service().step(&fixture.id).await.unwrap();
        let source = fixture.source.path().join("owned.pdf");
        if changed_tombstone {
            fs::write(
                fixture
                    .source
                    .path()
                    .join(format!(".library-move-{}/source", fixture.id)),
                b"changed",
            )
            .unwrap();
            assert!(matches!(
                fixture.service().recover(&fixture.id).await,
                Err(ImportError::Changed)
            ));
            assert_eq!(fs::read(&source).unwrap(), b"changed");
            assert_eq!(
                fixture
                    .service()
                    .get(&fixture.id)
                    .await
                    .unwrap()
                    .source_cleanup,
                SourceCleanup::Pending
            );
        } else {
            fs::write(&source, b"new original name").unwrap();
            assert_eq!(
                fixture.service().recover(&fixture.id).await.unwrap().phase,
                ImportPhase::Done
            );
            assert_eq!(fs::read(&source).unwrap(), b"new original name");
        }
        assert!(
            !fixture
                .source
                .path()
                .join(format!(".library-move-{}", fixture.id))
                .exists()
        );
        assert_eq!(
            fs::read(fixture.destination.path().join("imported.pdf")).unwrap(),
            PDF
        );
    }
}

#[tokio::test]
async fn operator_can_abandon_changed_move_cleanup_only_with_valid_destination_evidence() {
    let mut fixture = pending_move().await;
    fs::write(
        fixture.source.path().join("owned.pdf"),
        b"keep changed source",
    )
    .unwrap();
    assert!(matches!(
        fixture.service().recover(&fixture.id).await,
        Err(ImportError::Changed)
    ));
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("DELETE FROM file_coverage")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        fixture.service().abandon_move_cleanup(&fixture.id).await,
        Err(ImportError::InvalidJournal)
    ));
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES(?,'unit','user_confirmed')").bind(&fixture.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let resolved = fixture
        .service()
        .abandon_move_cleanup(&fixture.id)
        .await
        .unwrap();
    assert_eq!(resolved.phase, ImportPhase::Done);
    assert_eq!(resolved.source_cleanup, SourceCleanup::Abandoned);
    fixture.store.close().await;
    fixture.store = SqliteStore::open(fixture.state.path()).await.unwrap();
    assert_eq!(
        fixture
            .service()
            .recover(&fixture.id)
            .await
            .unwrap()
            .source_cleanup,
        SourceCleanup::Abandoned
    );
    assert_eq!(
        fixture
            .service()
            .abandon_move_cleanup(&fixture.id)
            .await
            .unwrap()
            .source_cleanup,
        SourceCleanup::Abandoned
    );
    assert_eq!(
        fs::read(fixture.source.path().join("owned.pdf")).unwrap(),
        b"keep changed source"
    );
    assert_eq!(
        fs::read(fixture.destination.path().join("imported.pdf")).unwrap(),
        PDF
    );
    let copy = Fixture::new(ImportPolicy::Copy).await;
    copy.plan().await;
    for _ in 0..5 {
        copy.service().step(&copy.id).await.unwrap();
    }
    assert!(matches!(
        copy.service().abandon_move_cleanup(&copy.id).await,
        Err(ImportError::Conflict)
    ));
}

#[tokio::test]
async fn move_rejects_other_imports_reserved_and_published_destinations() {
    for boundary in ["plan", "replay", "cleanup"] {
        for published in [false, true] {
            let fixture = if boundary == "cleanup" {
                pending_move().await
            } else {
                Fixture::new(ImportPolicy::Move).await
            };
            if boundary == "replay" {
                fixture.plan().await;
            }
            let source = fixture.source.path().join("owned.pdf");
            let saved = fixture.source.path().join("saved.pdf");
            fs::rename(&source, &saved).unwrap();
            fs::write(fixture.destination.path().join("other.pdf"), PDF).unwrap();
            let other_id = Uuid::new_v4().to_string();
            fixture
                .service()
                .plan(
                    &other_id,
                    InternalImportRequest {
                        source_root: fixture.request.destination_root.clone(),
                        source_relative: "other.pdf".into(),
                        destination_root: fixture.request.source_root.clone(),
                        destination_relative: "owned.pdf".into(),
                        unit_id: fixture.request.unit_id.clone(),
                        policy: ImportPolicy::Copy,
                    },
                    ImportMode::Standalone,
                )
                .await
                .unwrap();
            if published {
                for _ in 0..3 {
                    fixture.service().step(&other_id).await.unwrap();
                }
            } else {
                fs::rename(&saved, &source).unwrap();
            }
            let result = if boundary == "cleanup" {
                fixture.service().recover(&fixture.id).await
            } else {
                fixture
                    .service()
                    .plan(&fixture.id, fixture.request.clone(), ImportMode::Standalone)
                    .await
            };
            assert!(
                matches!(result, Err(ImportError::ProtectedSource)),
                "{boundary}, published={published}: {result:?}"
            );
            assert_eq!(fs::read(source).unwrap(), PDF);
        }
    }
}

#[tokio::test]
async fn recovery_removes_empty_move_directories_and_preserves_nonempty_ones() {
    for completed in [false, true] {
        for nonempty in [false, true] {
            let fixture = pending_move().await;
            if completed {
                fixture.assert_done().await;
            }
            let directory = fixture
                .source
                .path()
                .join(format!(".library-move-{}", fixture.id));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            if nonempty {
                fs::write(directory.join("keep"), b"unrelated").unwrap();
            }
            fixture.assert_done().await;
            assert_eq!(directory.exists(), nonempty);
            if nonempty {
                assert_eq!(fs::read(directory.join("keep")).unwrap(), b"unrelated");
            }
        }
    }
}

#[tokio::test]
async fn abandonment_keeps_sources_when_the_parent_is_renamed_or_replaced() {
    for parent_change in ["intact", "renamed", "replaced"] {
        for captured in [false, true] {
            let mut fixture = Fixture::new(ImportPolicy::Move).await;
            let parent = fixture.source.path().join("nested");
            fs::create_dir(&parent).unwrap();
            fs::rename(
                fixture.source.path().join("owned.pdf"),
                parent.join("owned.pdf"),
            )
            .unwrap();
            fixture.request.source_relative = "nested/owned.pdf".into();
            fixture.plan().await;
            for _ in 0..5 {
                fixture.service().step(&fixture.id).await.unwrap();
            }
            if captured {
                fixture.service().step(&fixture.id).await.unwrap();
            }
            let kept_parent = if parent_change == "intact" {
                parent.clone()
            } else {
                let kept = fixture.source.path().join("kept");
                fs::rename(&parent, &kept).unwrap();
                if parent_change == "replaced" {
                    fs::create_dir(&parent).unwrap();
                    fs::write(parent.join("owned.pdf"), b"replacement").unwrap();
                    if captured {
                        let private = parent.join(format!(".library-move-{}", fixture.id));
                        fs::create_dir(&private).unwrap();
                        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
                        fs::write(private.join("source"), b"replacement tombstone").unwrap();
                    }
                }
                kept
            };
            let done = fixture
                .service()
                .abandon_move_cleanup(&fixture.id)
                .await
                .unwrap();
            assert_eq!(done.phase, ImportPhase::Done);
            assert_eq!(done.source_cleanup, SourceCleanup::Abandoned);
            if parent_change == "replaced" && captured {
                assert!(done.last_error.is_some());
                assert_eq!(
                    fs::read(parent.join(format!(".library-move-{}/source", fixture.id))).unwrap(),
                    b"replacement tombstone"
                );
            }
            let retained = if captured && parent_change != "intact" {
                kept_parent.join(format!(".library-move-{}/source", fixture.id))
            } else {
                kept_parent.join("owned.pdf")
            };
            assert_eq!(fs::read(&retained).unwrap(), PDF);
            if parent_change == "intact" {
                assert!(
                    !parent
                        .join(format!(".library-move-{}", fixture.id))
                        .exists()
                );
            }
            if parent_change == "replaced" {
                assert_eq!(fs::read(parent.join("owned.pdf")).unwrap(), b"replacement");
            }
            assert_eq!(
                fixture.service().recover(&fixture.id).await.unwrap().phase,
                ImportPhase::Done
            );
            assert_eq!(
                fixture
                    .service()
                    .abandon_move_cleanup(&fixture.id)
                    .await
                    .unwrap()
                    .phase,
                ImportPhase::Done
            );
            assert_eq!(fs::read(retained).unwrap(), PDF);
        }
    }
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
