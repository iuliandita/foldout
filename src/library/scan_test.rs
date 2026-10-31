use std::{fs, path::Path};

use tempfile::tempdir;

use super::roots::Library;
use crate::store::sqlite::SqliteStore;

async fn library() -> (tempfile::TempDir, Library) {
    let state = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(state.path()).await.unwrap();
    (state, Library::new(store))
}
fn write(root: &Path, name: &str, contents: &[u8]) {
    fs::write(root.join(name), contents).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn interrupted_scan_restart_reuses_committed_hashes_and_finishes_inventory() {
    use std::{collections::BTreeMap, sync::Arc};

    use super::scan::ScanCommitGate;

    let (_state, mut library) = library().await;
    let root = tempdir().unwrap();
    let fixtures: Vec<String> = (0..65).map(|index| format!("{index:02}.cbz")).collect();
    for name in &fixtures {
        write(root.path(), name, name.as_bytes());
    }
    std::os::unix::fs::symlink("00.cbz", root.path().join("link.cbz")).unwrap();
    let registered = library.register_root("restart", root.path()).await.unwrap();
    let gate = Arc::new(ScanCommitGate {
        committed: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let (observer, mut first_hashes) = tokio::sync::mpsc::unbounded_channel();
    library.hash_observer = Some(observer);
    library.scan_commit_gate = Some(gate.clone());
    let scan = tokio::spawn({
        let library = library.clone();
        let root_id = registered.id.clone();
        async move { library.scan(&root_id).await }
    });
    gate.committed.notified().await;
    let committed: BTreeMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT relative_path, signature FROM scan_entries WHERE root_id = ? AND state = 'pending_association'",
    )
    .bind(&registered.id)
    .fetch_all(library.store.reader())
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert!(!committed.is_empty());
    assert!(committed.len() < fixtures.len());
    let mut observed = Vec::new();
    while let Ok(path) = first_hashes.try_recv() {
        observed.push(path);
    }
    observed.sort();
    assert_eq!(observed, committed.keys().cloned().collect::<Vec<_>>());
    assert!(committed.values().all(|signature| signature.len() == 64));
    scan.abort();
    assert!(scan.await.unwrap_err().is_cancelled());
    let store = library.store.clone();
    drop(library);

    let mut restarted = Library::new(store);
    let (observer, mut restart_hashes) = tokio::sync::mpsc::unbounded_channel();
    restarted.hash_observer = Some(observer);
    let report = restarted.scan(&registered.id).await.unwrap();
    let mut observed = Vec::new();
    while let Ok(path) = restart_hashes.try_recv() {
        observed.push(path);
    }
    observed.sort();
    let remaining: Vec<_> = fixtures
        .iter()
        .filter(|path| !committed.contains_key(*path))
        .cloned()
        .collect();
    assert_eq!(observed, remaining);
    assert_eq!(report.visited, fixtures.len() as u64 + 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.errors, 0);

    let entries = restarted
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    assert_eq!(entries.total, fixtures.len() as i64 + 1);
    assert!(entries.next_cursor.is_none());
    let mut inventoried: Vec<_> = entries
        .items
        .iter()
        .filter(|entry| entry.state == "pending_association")
        .map(|entry| entry.relative_path.clone())
        .collect();
    inventoried.sort();
    assert_eq!(inventoried, fixtures);
    let skipped: Vec<_> = entries
        .items
        .iter()
        .filter(|entry| entry.state == "skipped")
        .collect();
    assert_eq!(skipped.len(), 1);
    assert!(skipped.iter().all(|entry| {
        entry
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty())
    }));
    assert_eq!(skipped[0].relative_path, "link.cbz");
    assert_eq!(skipped[0].reason.as_deref(), Some("symlink_skipped"));
    for (path, signature) in committed {
        let entry = entries
            .items
            .iter()
            .find(|entry| entry.relative_path == path)
            .unwrap();
        assert_eq!(entry.reason.as_deref(), Some("metadata_unchanged"));
        let reused: String = sqlx::query_scalar(
            "SELECT signature FROM scan_entries WHERE root_id = ? AND relative_path = ?",
        )
        .bind(&registered.id)
        .bind(&path)
        .fetch_one(restarted.store.reader())
        .await
        .unwrap();
        assert_eq!(reused, signature);
    }
}

#[tokio::test]
async fn copied_files_keep_their_original_bytes_and_unchanged_scan_reuses_hash() {
    let (_state, library) = library().await;
    let source = tempdir().unwrap();
    let root = tempdir().unwrap();
    write(source.path(), "sample.cbz", b"original");
    fs::copy(
        source.path().join("sample.cbz"),
        root.path().join("sample.cbz"),
    )
    .unwrap();
    let original = fs::read(root.path().join("sample.cbz")).unwrap();
    let registered = library
        .register_root("fixtures", root.path())
        .await
        .unwrap();
    library.scan(&registered.id).await.unwrap();
    let first: String = sqlx::query_scalar("SELECT signature FROM scan_entries")
        .fetch_one(library.store.reader())
        .await
        .unwrap();
    library.scan(&registered.id).await.unwrap();
    let reason: String = sqlx::query_scalar("SELECT reason FROM scan_entries")
        .fetch_one(library.store.reader())
        .await
        .unwrap();
    assert_eq!(fs::read(root.path().join("sample.cbz")).unwrap(), original);
    assert_eq!(reason, "metadata_unchanged");
    assert_eq!(first.len(), 64);
}

#[tokio::test]
async fn changed_file_is_rehashed_and_symlink_escape_is_skipped() {
    let (_state, library) = library().await;
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    write(root.path(), "a.pdf", b"one");
    write(outside.path(), "outside.pdf", b"outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("outside.pdf"),
        root.path().join("escape.pdf"),
    )
    .unwrap();
    let registered = library.register_root("root", root.path()).await.unwrap();
    library.scan(&registered.id).await.unwrap();
    let first: String =
        sqlx::query_scalar("SELECT signature FROM scan_entries WHERE relative_path = 'a.pdf'")
            .fetch_one(library.store.reader())
            .await
            .unwrap();
    write(root.path(), "a.pdf", b"two-changed");
    library.scan(&registered.id).await.unwrap();
    let second: String =
        sqlx::query_scalar("SELECT signature FROM scan_entries WHERE relative_path = 'a.pdf'")
            .fetch_one(library.store.reader())
            .await
            .unwrap();
    assert_ne!(first, second);
    #[cfg(unix)]
    {
        let reason: String = sqlx::query_scalar(
            "SELECT reason FROM scan_entries WHERE relative_path = 'escape.pdf'",
        )
        .fetch_one(library.store.reader())
        .await
        .unwrap();
        assert_eq!(reason, "symlink_skipped");
    }
}

#[tokio::test]
async fn unreadable_folder_is_recorded_explicitly() {
    let (_state, library) = library().await;
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("closed")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            root.path().join("closed"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    let registered = library.register_root("root", root.path()).await.unwrap();
    let report = library.scan(&registered.id).await.unwrap();
    #[cfg(unix)]
    {
        let reason: Option<String> =
            sqlx::query_scalar("SELECT reason FROM scan_entries WHERE relative_path = 'closed'")
                .fetch_optional(library.store.reader())
                .await
                .unwrap();
        if reason.is_some() {
            assert_eq!(reason.as_deref(), Some("directory_unreadable"));
            assert_eq!(report.errors, 1);
        }
    }
}

#[tokio::test]
async fn missing_root_fails_the_scan() {
    let (_state, library) = library().await;
    assert!(library.scan("missing").await.is_err());
}

#[tokio::test]
async fn inventory_associations_follow_current_content_without_consuming_scan_state() {
    use crate::{
        catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
        importer::preview::PreviewService,
    };
    let (_state, library) = library().await;
    let root = tempdir().unwrap();
    write(root.path(), "collection.cbz", b"first");
    write(root.path(), "copy.cbz", b"first");
    let registered = library.register_root("fixture", root.path()).await.unwrap();
    library.scan(&registered.id).await.unwrap();
    let catalog = CatalogRepository::new(library.store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type: ContentType::Comic,
            title: "Collection".into(),
            sort_title: None,
            run_label: None,
            known_unit_count: None,
        })
        .await
        .unwrap();
    let edition = catalog
        .create_edition(NewEdition {
            publication_id: publication.id,
            language: "en".into(),
            region: None,
            publisher: None,
        })
        .await
        .unwrap();
    let initial = library
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    assert!(
        initial
            .items
            .iter()
            .all(|entry| entry.associated_unit_count == 0)
    );
    let entry = initial
        .items
        .iter()
        .find(|entry| entry.relative_path == "collection.cbz")
        .unwrap();
    let previews = PreviewService::new(library.store.clone());
    for (index, label) in ["1", "2"].into_iter().enumerate() {
        let unit = catalog
            .create_unit(NewUnit {
                edition_id: edition.id.clone(),
                label: label.into(),
                kind: UnitKind::Issue,
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
        for _ in 0..2 {
            let preview = previews.preview(&entry.id, &unit.id).await.unwrap();
            previews.accept(&preview.id).await.unwrap();
            previews.accept(&preview.id).await.unwrap();
            let page = library
                .inventory_entries(&registered.id, None, 100)
                .await
                .unwrap();
            let associated = page.items.iter().find(|item| item.id == entry.id).unwrap();
            assert_eq!(associated.associated_unit_count, index as i64 + 1);
            assert_eq!(
                associated
                    .associated_units
                    .iter()
                    .map(|unit| unit.unit_label.as_str())
                    .collect::<Vec<_>>(),
                ["1", "2"][..=index]
            );
            assert!(associated.associated_units.iter().all(|unit| {
                unit.publication_title == "Collection" && unit.unit_kind == "issue"
            }));
            assert_eq!(associated.state, entry.state);
            assert_eq!(associated.reason, entry.reason);
            assert_eq!(
                page.items
                    .iter()
                    .find(|item| item.relative_path == "copy.cbz")
                    .unwrap()
                    .associated_unit_count,
                0
            );
        }
    }
    library.scan(&registered.id).await.unwrap();
    let page = library
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .find(|item| item.id == entry.id)
            .unwrap()
            .associated_unit_count,
        2
    );
    // Same size, different hash must invalidate the derived association.
    write(root.path(), "collection.cbz", b"other");
    fs::File::open(root.path().join("collection.cbz"))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100))
        .unwrap();
    library.scan(&registered.id).await.unwrap();
    let page = library
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .find(|item| item.id == entry.id)
            .unwrap()
            .associated_unit_count,
        0
    );
    write(root.path(), "collection.cbz", b"first");
    library.scan(&registered.id).await.unwrap();
    assert_eq!(
        library
            .inventory_entries(&registered.id, None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .find(|item| item.id == entry.id)
            .unwrap()
            .associated_unit_count,
        2
    );
    // Simulate non-ready scanner observations retaining otherwise matching metadata.
    for state in ["error", "skipped"] {
        let mut tx = library.store.begin_write().await.unwrap();
        sqlx::query("UPDATE scan_entries SET state = ? WHERE id = ?")
            .bind(state)
            .bind(&entry.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            library
                .inventory_entries(&registered.id, None, 100)
                .await
                .unwrap()
                .items
                .iter()
                .find(|item| item.id == entry.id)
                .unwrap()
                .associated_unit_count,
            0
        );
    }
    library.scan(&registered.id).await.unwrap();
    // Matching signature alone is insufficient if the catalog's size differs.
    let mut tx = library.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET size_bytes = size_bytes + 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        library
            .inventory_entries(&registered.id, None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .find(|item| item.id == entry.id)
            .unwrap()
            .associated_unit_count,
        0
    );
    let mut tx = library.store.begin_write().await.unwrap();
    sqlx::query("UPDATE library_files SET size_bytes = size_bytes - 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    fs::remove_file(root.path().join("collection.cbz")).unwrap();
    library.scan(&registered.id).await.unwrap();
    let page = library
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    let missing = page.items.iter().find(|item| item.id == entry.id).unwrap();
    assert_eq!(missing.state, "missing");
    assert_eq!(missing.associated_unit_count, 0);
    let coverage: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_coverage")
        .fetch_one(library.store.reader())
        .await
        .unwrap();
    assert_eq!(
        coverage, 2,
        "listing and scanning preserve collection coverage"
    );
    write(root.path(), "collection.cbz", b"first");
    library.scan(&registered.id).await.unwrap();
    for label in ["7", "6", "5", "4", "3"] {
        let unit = catalog
            .create_unit(NewUnit {
                edition_id: edition.id.clone(),
                label: label.into(),
                kind: UnitKind::Volume,
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
        let mut tx = library.store.begin_write().await.unwrap();
        sqlx::query(
            "INSERT INTO file_coverage(library_file_id,unit_id,evidence)
             SELECT library_file_id, ?, 'user_confirmed' FROM file_coverage LIMIT 1",
        )
        .bind(&unit.id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let page = library
        .inventory_entries(&registered.id, None, 100)
        .await
        .unwrap();
    let capped = page.items.iter().find(|item| item.id == entry.id).unwrap();
    assert_eq!(capped.associated_unit_count, 7);
    assert_eq!(
        capped
            .associated_units
            .iter()
            .map(|unit| unit.unit_label.as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "3", "4", "5"]
    );
    assert!(
        page.items
            .iter()
            .filter(|item| item.id != entry.id)
            .all(|item| item.associated_units.is_empty())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn scan_persists_identity_hints_without_changing_original_or_coverage() {
    use sha2::{Digest, Sha256};
    use std::process::Command;
    let (_state, library) = library().await;
    let root = tempdir().unwrap();
    let source = tempdir().unwrap();
    fs::create_dir(source.path().join("metadata")).unwrap();
    fs::write(source.path().join("metadata/ComicInfo.xml"), b"<ComicInfo><ISBN>9780306406157</ISBN><MangaDexID>f9c33607-9180-4ba6-b85c-e4b5faee7192</MangaDexID></ComicInfo>").unwrap();
    let archive = root.path().join("book.cbz");
    assert!(
        Command::new("7z")
            .current_dir(source.path())
            .args(["a", "-tzip", "-bd", "-y"])
            .arg(&archive)
            .arg("metadata/ComicInfo.xml")
            .output()
            .unwrap()
            .status
            .success()
    );
    let before = Sha256::digest(fs::read(&archive).unwrap());
    let registered = library.register_root("fixture", root.path()).await.unwrap();
    let report = library.scan(&registered.id).await.unwrap();
    assert_eq!(report.errors, 0);
    let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as("SELECT kind,status,source_field,container_path,extracted_at FROM identity_hints ORDER BY kind").fetch_all(library.store.reader()).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (&*rows[0].0, &*rows[0].1, &*rows[0].2, &*rows[0].3),
        ("isbn", "usable", "ISBN", "metadata/ComicInfo.xml")
    );
    assert!(rows.iter().all(|row| row.4 > 0));
    let (entry_id, signature): (String, String) =
        sqlx::query_as("SELECT id,signature FROM scan_entries")
            .fetch_one(library.store.reader())
            .await
            .unwrap();
    let count: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM library_files),(SELECT COUNT(*) FROM file_coverage)",
    )
    .fetch_one(library.store.reader())
    .await
    .unwrap();
    assert_eq!(count, (0, 0));
    let mut tx = library.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path) VALUES('file',?,'cbz',?,?,?,'book.cbz')").bind(archive.to_str().unwrap()).bind(&signature).bind(fs::metadata(&archive).unwrap().len() as i64).bind(&registered.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        library
            .usable_identity_hints_for_file("file")
            .await
            .unwrap()
            .len(),
        2
    );
    library.scan(&registered.id).await.unwrap();
    let after: Vec<(String, String, String, String, i64)> = sqlx::query_as("SELECT kind,status,source_field,container_path,extracted_at FROM identity_hints WHERE entry_id=? ORDER BY kind").bind(entry_id).fetch_all(library.store.reader()).await.unwrap();
    assert_eq!(rows, after);
    assert_eq!(before, Sha256::digest(fs::read(&archive).unwrap()));

    fs::write(
        source.path().join("metadata/ComicInfo.xml"),
        b"<ComicInfo><ISBN>0306406152</ISBN></ComicInfo>",
    )
    .unwrap();
    let replacement = root.path().join("replacement.zip");
    assert!(
        Command::new("7z")
            .current_dir(source.path())
            .args(["a", "-tzip", "-bd", "-y"])
            .arg(&replacement)
            .arg("metadata/ComicInfo.xml")
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::rename(replacement, &archive).unwrap();
    let changed_hash = Sha256::digest(fs::read(&archive).unwrap());
    library.scan(&registered.id).await.unwrap();
    let updated: Vec<(String, String, String)> =
        sqlx::query_as("SELECT kind,raw_value,status FROM identity_hints ORDER BY kind")
            .fetch_all(library.store.reader())
            .await
            .unwrap();
    assert_eq!(
        updated,
        vec![("isbn".into(), "0306406152".into(), "usable".into())]
    );
    assert!(
        library
            .usable_identity_hints_for_file("file")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(changed_hash, Sha256::digest(fs::read(&archive).unwrap()));

    let mut tx = library.store.begin_write().await.unwrap();
    sqlx::query("UPDATE identity_hints SET status='needs_review', value=NULL")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    fs::write(
        source.path().join("metadata/ComicInfo.xml"),
        b"<ComicInfo><ISBN>9791090636071</ISBN></ComicInfo>",
    )
    .unwrap();
    let replacement = root.path().join("replacement.zip");
    assert!(
        Command::new("7z")
            .current_dir(source.path())
            .args(["a", "-tzip", "-bd", "-y"])
            .arg(&replacement)
            .arg("metadata/ComicInfo.xml")
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::rename(replacement, &archive).unwrap();
    let latest_hash = Sha256::digest(fs::read(&archive).unwrap());
    library.scan(&registered.id).await.unwrap();
    let status: (String, Option<String>) =
        sqlx::query_as("SELECT status,value FROM identity_hints")
            .fetch_one(library.store.reader())
            .await
            .unwrap();
    assert_eq!(status, ("needs_review".into(), None));
    assert_eq!(latest_hash, Sha256::digest(fs::read(&archive).unwrap()));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_identity_extraction_retries_with_backoff_and_keeps_original() {
    use sha2::{Digest, Sha256};
    use std::process::Command;
    let (_state, library) = library().await;
    let root = tempdir().unwrap();
    let source = tempdir().unwrap();
    let archive = root.path().join("bad.cbz");
    fs::write(
        source.path().join("ComicInfo.xml"),
        vec![b' '; crate::reader::archive::MAX_COMIC_INFO_BYTES + 1],
    )
    .unwrap();
    assert!(
        Command::new("7z")
            .current_dir(source.path())
            .args(["a", "-tzip", "-bd", "-y"])
            .arg(&archive)
            .arg("ComicInfo.xml")
            .output()
            .unwrap()
            .status
            .success()
    );
    let original = Sha256::digest(fs::read(&archive).unwrap());
    let registered = library.register_root("fixture", root.path()).await.unwrap();
    assert_eq!(library.scan(&registered.id).await.unwrap().errors, 0);
    let first: (Option<String>, i64, i64) = sqlx::query_as(
        "SELECT hints_signature,hints_attempts,hints_next_attempt FROM scan_entries",
    )
    .fetch_one(library.store.reader())
    .await
    .unwrap();
    assert_eq!(first.0, None);
    assert_eq!(first.1, 1);
    assert!(first.2 > 0);
    assert_eq!(library.scan(&registered.id).await.unwrap().errors, 0);
    let next: (Option<String>, i64, i64) = sqlx::query_as(
        "SELECT hints_signature,hints_attempts,hints_next_attempt FROM scan_entries",
    )
    .fetch_one(library.store.reader())
    .await
    .unwrap();
    assert_eq!(first, next);
    let mut tx = library.store.begin_write().await.unwrap();
    sqlx::query("UPDATE scan_entries SET hints_next_attempt=0")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(library.scan(&registered.id).await.unwrap().errors, 0);
    let attempts: i64 = sqlx::query_scalar("SELECT hints_attempts FROM scan_entries")
        .fetch_one(library.store.reader())
        .await
        .unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(original, Sha256::digest(fs::read(&archive).unwrap()));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reader_metadata_ignores_invalid_identity_fields_in_archive() {
    use std::{process::Command, sync::Arc};
    let root = tempdir().unwrap();
    let source = tempdir().unwrap();
    let xml = format!(
        "<ComicInfo><Title>Readable</Title><ISBN>0306406152</ISBN><ISBN><nested>bad</nested></ISBN><EAN>{}</EAN></ComicInfo>",
        "9".repeat(5000)
    );
    fs::write(source.path().join("ComicInfo.xml"), xml).unwrap();
    let archive = root.path().join("book.cbz");
    assert!(
        Command::new("7z")
            .current_dir(source.path())
            .args(["a", "-tzip", "-bd", "-y"])
            .arg(&archive)
            .arg("ComicInfo.xml")
            .output()
            .unwrap()
            .status
            .success()
    );
    let decoder = crate::reader::archive::ArchiveDecoder::new();
    let metadata = crate::library::metadata::EmbeddedMetadata::read(
        &decoder,
        Arc::new(fs::File::open(&archive).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(metadata.title.as_deref(), Some("Readable"));
}
