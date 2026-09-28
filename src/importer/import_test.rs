use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::Path,
};

use tempfile::{TempDir, tempdir};
use uuid::Uuid;

use super::{
    finalize,
    journal::{ImportError, ImportPhase, ImportPolicy, ImportService, InternalImportRequest},
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
                .plan(&self.id, self.request.clone())
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
        assert_eq!(
            fs::read(self.source.path().join(&self.request.source_relative)).unwrap(),
            PDF
        );
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
        fixture.service().plan(&fixture.id, changed).await,
        Err(ImportError::Conflict)
    ));
    assert!(matches!(
        fixture
            .service()
            .plan(&Uuid::new_v4().to_string(), fixture.request.clone())
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
                .plan(&fixture.id, fixture.request.clone())
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
        ImportPolicy::Hardlink {
            fallback_to_copy: false,
        },
    ] {
        let fixture = Fixture::new(policy).await;
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
async fn changed_stage_or_final_inode_is_never_accepted() {
    for phase_count in [1, 3, 6] {
        let fixture = Fixture::new(ImportPolicy::Copy).await;
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
        fs::rename(&path, path.with_extension("preserved")).unwrap();
        fs::write(&path, PDF).unwrap();
        assert!(matches!(
            fixture.service().recover(&fixture.id).await,
            Err(ImportError::Changed)
        ));
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
            fixture.service().plan(&fixture.id, request).await,
            Err(ImportError::UnsafePath)
        ));
    }
    let outside = tempdir().unwrap();
    symlink(outside.path(), fixture.destination.path().join("link")).unwrap();
    fixture.request.destination_relative = "link/imported.pdf".into();
    assert!(
        fixture
            .service()
            .plan(&fixture.id, fixture.request.clone())
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
                .plan(&fixture.id, fixture.request.clone())
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

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap()
}
