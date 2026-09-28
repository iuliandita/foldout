use std::fs;

use tempfile::tempdir;

use crate::{
    catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
    importer::preview::{PreviewError, PreviewService},
    library::roots::Library,
    store::sqlite::SqliteStore,
};

async fn setup() -> (tempfile::TempDir, SqliteStore, String) {
    let state = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(state.path()).await.unwrap();
    let catalog = CatalogRepository::new(store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type: ContentType::Comic,
            title: "Catalog fixture".into(),
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
    let mut units = Vec::new();
    for label in ["1", "2", "3"] {
        units.push(
            catalog
                .create_unit(NewUnit {
                    edition_id: edition.id.clone(),
                    label: label.into(),
                    kind: UnitKind::Issue,
                    sort_key: None,
                    date: None,
                })
                .await
                .unwrap(),
        );
    }
    (state, store, units.remove(0).id)
}

#[tokio::test]
async fn stale_preview_fails_and_duplicate_accept_is_idempotent() {
    let (_state, store, unit_id) = setup().await;
    let root = tempdir().unwrap();
    fs::write(root.path().join("fixture.cbz"), b"first").unwrap();
    let library = Library::new(store.clone());
    let registered = library.register_root("fixture", root.path()).await.unwrap();
    library.scan(&registered.id).await.unwrap();
    let entry: String = sqlx::query_scalar("SELECT id FROM scan_entries WHERE root_id = ?")
        .bind(&registered.id)
        .fetch_one(store.reader())
        .await
        .unwrap();
    let previews = PreviewService::new(store.clone());
    let stale = previews.preview(&entry, &unit_id).await.unwrap();
    fs::write(root.path().join("fixture.cbz"), b"other").unwrap();
    assert!(matches!(
        previews.accept(&stale.id).await,
        Err(PreviewError::Stale)
    ));
    library.scan(&registered.id).await.unwrap();
    let fresh = previews.preview(&entry, &unit_id).await.unwrap();
    let first = previews.accept(&fresh.id).await.unwrap();
    let second = previews.accept(&fresh.id).await.unwrap();
    assert_eq!(first.id, second.id);
    fs::write(root.path().join("fixture.cbz"), b"again").unwrap();
    assert!(matches!(
        previews.accept(&fresh.id).await,
        Err(PreviewError::Stale)
    ));
    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM library_files")
        .fetch_one(store.reader())
        .await
        .unwrap();
    let coverage: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_coverage")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!((files, coverage), (1, 1));
}
