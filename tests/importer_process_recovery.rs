//! Real SIGKILL recovery using only an owned test process/database and public importer APIs.
use libraryd::{
    importer::journal::{
        ImportError, ImportPhase, ImportPolicy, ImportService, InternalImportRequest,
    },
    library::roots::Library,
    reader::archive::ArchiveDecoder,
    store::sqlite::SqliteStore,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

const CBZ: &[u8] = include_bytes!("fixtures/natural-order.cbz");
const CHILD_ROOT: &str = "LIBRARY_IMPORT_RECOVERY_CHILD_ROOT";
const CHILD_NONCE: &str = "LIBRARY_IMPORT_RECOVERY_CHILD_NONCE";
const CHILD_TEST: &str = "importer_recovery_child";

#[derive(Serialize, Deserialize)]
struct Scenario {
    nonce: String,
    operation: String,
    boundary: String,
    request: InternalImportRequest,
}

fn nice() {
    assert_eq!(unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 8) }, 0);
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .unwrap()
}
fn private_file(path: &Path) -> File {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap()
}
fn identity(path: &Path) -> (u64, u64) {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_file());
    (meta.dev(), meta.ino())
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn diagnostics(root: &Path) -> String {
    let mut text = String::new();
    File::open(root.join("child.log"))
        .unwrap()
        .take(8192)
        .read_to_string(&mut text)
        .unwrap();
    text
}

// Ordinary harness invocation is inert; the parent explicitly selects this entry in its child.
#[test]
fn importer_recovery_child() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    nice();
    let root = PathBuf::from(root);
    let meta = fs::symlink_metadata(&root).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    assert_eq!(meta.mode() & 0o777, 0o700);
    let mut input = String::new();
    File::open(root.join("scenario.json"))
        .unwrap()
        .take(8192)
        .read_to_string(&mut input)
        .unwrap();
    let scenario: Scenario = serde_json::from_str(&input).unwrap();
    assert_eq!(std::env::var(CHILD_NONCE).unwrap(), scenario.nonce);
    runtime().block_on(async {
        let store = SqliteStore::open(&root.join("state")).await.unwrap();
        let service = ImportService::new(store.clone());
        let mut operation = service.plan(&scenario.operation, scenario.request).await.unwrap();
        let phase = match scenario.boundary.as_str() {
            "planned" => ImportPhase::Planned,
            "staged" => ImportPhase::Staged,
            "verified" | "published_uncommitted" => ImportPhase::Verified,
            "finalized" => ImportPhase::Finalized,
            "cataloged" => ImportPhase::Cataloged,
            "cleanup_pending" => ImportPhase::CleanupPending,
            _ => panic!("unknown child boundary"),
        };
        for _ in 0..6 {
            if operation.phase == phase { break; }
            operation = service.step(&scenario.operation).await.unwrap();
        }
        assert_eq!(operation.phase, phase);
        if scenario.boundary == "published_uncommitted" {
            // Test database only: force the post-publication phase UPDATE to affect zero rows.
            let mut tx = store.begin_write().await.unwrap();
            sqlx::query("CREATE TRIGGER test_hold_import_publication BEFORE UPDATE OF phase ON import_operations WHEN NEW.phase='finalized' BEGIN SELECT RAISE(IGNORE); END")
                .execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            assert!(matches!(service.step(&scenario.operation).await, Err(ImportError::Busy)));
            assert_eq!(service.get(&scenario.operation).await.unwrap().phase, ImportPhase::Verified);
            assert_eq!(fs::read(root.join("destination/imported.cbz")).unwrap(), CBZ);
        }
        // Publish the marker only after the intended durable state and all step tasks settled.
        let mut marker = private_file(&root.join("ready.pending"));
        marker.write_all(scenario.nonce.as_bytes()).unwrap();
        marker.sync_all().unwrap();
        fs::rename(root.join("ready.pending"), root.join("ready")).unwrap();
        File::open(&root).unwrap().sync_all().unwrap();
        // Keep SQLite/service handles alive; SIGKILL must prevent ordinary shutdown/destructors.
        std::future::pending::<()>().await;
        drop(service);
        store.close().await;
    });
}

async fn scenario(boundary: &str, hardlink: bool, collision: bool) {
    let temp = tempfile::Builder::new()
        .prefix("import-process-recovery-")
        .tempdir()
        .unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = temp.path();
    for name in ["state", "source", "destination"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join(name))
            .unwrap();
    }
    let mut source = private_file(&root.join("source/owned.cbz"));
    source.write_all(CBZ).unwrap();
    source.sync_all().unwrap();
    drop(source);
    let original = identity(&root.join("source/owned.cbz"));
    let store = SqliteStore::open(&root.join("state")).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO publications(id,content_type,title,sort_title) VALUES('publication','comic','Owned process fixture','Owned process fixture')").execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO editions(id,publication_id,language) VALUES('edition','publication','en')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO units(id,edition_id,label,kind) VALUES('unit','edition','1','issue')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let library = Library::new(store.clone());
    let input = Scenario {
        nonce: Uuid::new_v4().to_string(),
        operation: Uuid::new_v4().to_string(),
        boundary: boundary.into(),
        request: InternalImportRequest {
            source_root: library
                .register_root("source", &root.join("source"))
                .await
                .unwrap()
                .id,
            source_relative: "owned.cbz".into(),
            destination_root: library
                .register_root("destination", &root.join("destination"))
                .await
                .unwrap()
                .id,
            destination_relative: "imported.cbz".into(),
            unit_id: "unit".into(),
            policy: if hardlink {
                ImportPolicy::Hardlink {
                    fallback_to_copy: false,
                }
            } else {
                ImportPolicy::Copy
            },
        },
    };
    let mut config = private_file(&root.join("scenario.json"));
    config
        .write_all(&serde_json::to_vec(&input).unwrap())
        .unwrap();
    config.sync_all().unwrap();
    drop(config);
    store.close().await;
    drop(library);
    let log = private_file(&root.join("child.log"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(CHILD_ROOT, root)
        .env(CHILD_NONCE, &input.nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut child = ChildGuard(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if root.join("ready").exists() {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before {boundary}: {}",
            diagnostics(root)
        );
        assert!(
            Instant::now() < deadline,
            "child timed out at {boundary}: {}",
            diagnostics(root)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        fs::read(root.join("ready")).unwrap(),
        input.nonce.as_bytes()
    );
    assert_eq!(
        fs::metadata(root.join("ready")).unwrap().mode() & 0o777,
        0o600
    );
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    let store = SqliteStore::open(&root.join("state")).await.unwrap();
    let service = ImportService::new(store.clone());
    let operation = service.get(&input.operation).await.unwrap();
    let expected_phase = if boundary == "published_uncommitted" {
        "verified"
    } else {
        boundary
    };
    assert_eq!(operation.phase.as_str(), expected_phase);
    let target = root.join("destination/imported.cbz");
    let published = matches!(
        boundary,
        "published_uncommitted" | "finalized" | "cataloged" | "cleanup_pending"
    );
    assert_eq!(target.exists(), published);
    let published_identity = published.then(|| identity(&target));
    let before_counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM library_files),(SELECT count(*) FROM file_coverage)",
    )
    .fetch_one(store.reader())
    .await
    .unwrap();
    let cataloged = matches!(boundary, "cataloged" | "cleanup_pending");
    assert_eq!(before_counts, if cataloged { (1, 1) } else { (0, 0) });
    if matches!(
        boundary,
        "staged" | "verified" | "published_uncommitted" | "finalized"
    ) {
        assert_eq!(
            fs::read(
                root.join("destination/.library-imports")
                    .join(format!("{}.tmp", input.operation))
            )
            .unwrap(),
            CBZ
        );
    }

    if boundary == "published_uncommitted" {
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("DROP TRIGGER test_hold_import_publication")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    if collision {
        assert!(!published);
        let mut existing = private_file(&target);
        existing
            .write_all(b"unrelated existing destination")
            .unwrap();
        existing.sync_all().unwrap();
        let existing_identity = identity(&target);
        for _ in 0..2 {
            assert!(matches!(
                service.recover(&input.operation).await,
                Err(ImportError::Conflict)
            ));
        }
        assert_eq!(
            fs::read(&target).unwrap(),
            b"unrelated existing destination"
        );
        assert_eq!(identity(&target), existing_identity);
        assert_eq!(
            service.get(&input.operation).await.unwrap().phase,
            operation.phase
        );
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM library_files),(SELECT count(*) FROM file_coverage)",
        )
        .fetch_one(store.reader())
        .await
        .unwrap();
        assert_eq!(counts, (0, 0));
    } else {
        let done = service.recover(&input.operation).await.unwrap();
        assert_eq!(done.phase, ImportPhase::Done);
        assert_eq!(
            service
                .recover(&input.operation)
                .await
                .unwrap()
                .library_file_id,
            done.library_file_id
        );
        assert_eq!(fs::read(&target).unwrap(), CBZ);
        if let Some(inode) = published_identity {
            assert_eq!(identity(&target), inode);
        }
        assert_eq!(identity(&target) == original, hardlink);
        assert_eq!(
            ArchiveDecoder::new()
                .manifest(&target)
                .await
                .unwrap()
                .pages
                .len(),
            3
        );
        let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM import_operations),(SELECT count(*) FROM library_files),(SELECT count(*) FROM file_coverage WHERE unit_id='unit')").fetch_one(store.reader()).await.unwrap();
        assert_eq!(counts, (1, 1, 1));
        assert!(
            !root
                .join("destination/.library-imports")
                .join(format!("{}.tmp", input.operation))
                .exists()
        );
        assert_eq!(
            fs::read_dir(root.join("destination"))
                .unwrap()
                .map(Result::unwrap)
                .filter(|e| e.file_type().unwrap().is_file())
                .count(),
            1
        );
    }
    assert_eq!(fs::read(root.join("source/owned.cbz")).unwrap(), CBZ);
    assert_eq!(identity(&root.join("source/owned.cbz")), original);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.reader())
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await;
    drop(service);
    drop(child);
    temp.close().unwrap();
}

#[test]
fn killed_importer_recovers_durable_and_published_boundaries_without_overwrite() {
    nice();
    runtime().block_on(async {
        // One child at a time; tiny owned archive, no clients/network or elevated privileges.
        for hardlink in [false, true] {
            for boundary in [
                "planned",
                "staged",
                "verified",
                "published_uncommitted",
                "finalized",
                "cataloged",
                "cleanup_pending",
            ] {
                tokio::time::timeout(Duration::from_secs(90), scenario(boundary, hardlink, false))
                    .await
                    .expect("bounded recovery scenario");
            }
            tokio::time::timeout(
                Duration::from_secs(90),
                scenario("verified", hardlink, true),
            )
            .await
            .expect("bounded collision scenario");
        }
    });
}
