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
