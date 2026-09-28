use super::*;
use crate::store::sqlite::SqliteStore;

async fn repository() -> (tempfile::TempDir, CatalogRepository) {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    (directory, CatalogRepository::new(store))
}

fn publication(title: &str, run_label: Option<&str>) -> NewPublication {
    NewPublication {
        content_type: ContentType::Comic,
        title: title.into(),
        sort_title: None,
        run_label: run_label.map(str::to_owned),
        known_unit_count: None,
    }
}

#[tokio::test]
async fn same_title_runs_remain_distinct_and_manual_titles_lock() {
    let (_directory, repository) = repository().await;
    let first = repository
        .create_publication(publication("The Flash", Some("1987")))
        .await
        .unwrap();
    let second = repository
        .create_publication(publication("The Flash", Some("2016")))
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    let changed = repository
        .update_publication(
            &first.id,
            PublicationUpdate {
                title: Some("Flash".into()),
                sort_title: None,
                run_label: None,
                known_unit_count: None,
            },
        )
        .await
        .unwrap();
    assert!(changed.title_locked);
    assert_eq!(
        repository
            .refresh_title(&first.id, "The Flash (metadata)")
            .await
            .unwrap()
            .title,
        "Flash"
    );
}

#[tokio::test]
async fn preserves_textual_fractional_and_numeric_unit_labels() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Saga", None))
        .await
        .unwrap();
    let edition = repository
        .create_edition(NewEdition {
            publication_id: publication.id,
            language: "en".into(),
            region: Some("US".into()),
            publisher: None,
        })
        .await
        .unwrap();
    for label in ["1/2", "12.5", "2024", "Annual 1"] {
        repository
            .create_unit(NewUnit {
                edition_id: edition.id.clone(),
                label: label.into(),
                kind: UnitKind::Issue,
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(
        repository
            .list_units(&edition.id)
            .await
            .unwrap()
            .into_iter()
            .map(|unit| unit.label)
            .collect::<Vec<_>>(),
        ["1/2", "12.5", "2024", "Annual 1"]
    );
}

#[tokio::test]
async fn editions_distinguish_translations_and_regional_magazines() {
    let (_directory, repository) = repository().await;
    let manga = repository
        .create_publication(NewPublication {
            content_type: ContentType::Manga,
            title: "Witch Hat Atelier".into(),
            sort_title: None,
            run_label: None,
            known_unit_count: None,
        })
        .await
        .unwrap();
    repository
        .create_edition(NewEdition {
            publication_id: manga.id.clone(),
            language: "ja".into(),
            region: Some("JP".into()),
            publisher: None,
        })
        .await
        .unwrap();
    repository
        .create_edition(NewEdition {
            publication_id: manga.id.clone(),
            language: "en".into(),
            region: Some("US".into()),
            publisher: None,
        })
        .await
        .unwrap();
    let magazine = repository
        .create_publication(NewPublication {
            content_type: ContentType::Magazine,
            title: "Edge".into(),
            sort_title: None,
            run_label: None,
            known_unit_count: None,
        })
        .await
        .unwrap();
    repository
        .create_edition(NewEdition {
            publication_id: magazine.id.clone(),
            language: "en".into(),
            region: Some("GB".into()),
            publisher: None,
        })
        .await
        .unwrap();
    repository
        .create_edition(NewEdition {
            publication_id: magazine.id.clone(),
            language: "en".into(),
            region: Some("US".into()),
            publisher: None,
        })
        .await
        .unwrap();
    assert_eq!(repository.list_editions(&manga.id).await.unwrap().len(), 2);
    assert_eq!(
        repository.list_editions(&magazine.id).await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn supports_special_combined_unknown_and_zero_known_counts() {
    let (_directory, repository) = repository().await;
    let unknown = repository
        .create_publication(publication("Unknown Collection", None))
        .await
        .unwrap();
    assert_eq!(unknown.known_unit_count, None);
    let zero = repository
        .update_publication(
            &unknown.id,
            PublicationUpdate {
                title: None,
                sort_title: None,
                run_label: None,
                known_unit_count: Some(Some(0)),
            },
        )
        .await
        .unwrap();
    assert_eq!(zero.known_unit_count, Some(0));
    let edition = repository
        .create_edition(NewEdition {
            publication_id: zero.id,
            language: "en".into(),
            region: None,
            publisher: None,
        })
        .await
        .unwrap();
    let unit = repository
        .create_unit(NewUnit {
            edition_id: edition.id,
            label: "July/August 2024".into(),
            kind: UnitKind::Combined,
            sort_key: None,
            date: Some("2024-07".into()),
        })
        .await
        .unwrap();
    assert_eq!(unit.kind, UnitKind::Combined);
    assert_eq!(unit.date_precision, Some(DatePrecision::Month));
    assert!(matches!(
        repository
            .create_unit(NewUnit {
                edition_id: unit.edition_id,
                label: "bad".into(),
                kind: UnitKind::Special,
                sort_key: None,
                date: Some("2024-02-30".into())
            })
            .await,
        Err(CatalogError::Invalid(_))
    ));
}

#[tokio::test]
async fn publication_update_distinguishes_absent_and_null_fields() {
    let (_directory, repository) = repository().await;
    let created = repository
        .create_publication(NewPublication {
            content_type: ContentType::Comic,
            title: "Mutable".into(),
            sort_title: None,
            run_label: Some("run".into()),
            known_unit_count: Some(0),
        })
        .await
        .unwrap();
    let absent: PublicationUpdate =
        serde_json::from_value(serde_json::json!({ "title": "Renamed" })).unwrap();
    let preserved = repository
        .update_publication(&created.id, absent)
        .await
        .unwrap();
    assert_eq!(preserved.run_label.as_deref(), Some("run"));
    assert_eq!(preserved.known_unit_count, Some(0));
    let clear: PublicationUpdate =
        serde_json::from_value(serde_json::json!({ "run_label": null, "known_unit_count": null }))
            .unwrap();
    let cleared = repository
        .update_publication(&created.id, clear)
        .await
        .unwrap();
    assert_eq!(cleared.run_label, None);
    assert_eq!(cleared.known_unit_count, None);
}

#[tokio::test]
async fn edition_and_unit_updates_preserve_associations_and_validate_values() {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let repository = CatalogRepository::new(store.clone());
    let publication = repository
        .create_publication(publication("Mutable", None))
        .await
        .unwrap();
    let edition = repository
        .create_edition(NewEdition {
            publication_id: publication.id.clone(),
            language: "en".into(),
            region: Some("GB".into()),
            publisher: Some("Press".into()),
        })
        .await
        .unwrap();
    let unit = repository
        .create_unit(NewUnit {
            edition_id: edition.id.clone(),
            label: "1".into(),
            kind: UnitKind::Issue,
            sort_key: Some("001".into()),
            date: Some("2024".into()),
        })
        .await
        .unwrap();
    repository
        .create_provider_link(NewProviderLink {
            provider: "source".into(),
            external_id: "unit-1".into(),
            publication_id: None,
            edition_id: None,
            unit_id: Some(unit.id.clone()),
        })
        .await
        .unwrap();
    repository
        .create_provider_link(NewProviderLink {
            provider: "source".into(),
            external_id: "edition-1".into(),
            publication_id: None,
            edition_id: Some(edition.id.clone()),
            unit_id: None,
        })
        .await
        .unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES ('file','/sample.cbz','cbz','signature',42)")
        .execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES ('file',?,'user_confirmed')")
        .bind(&unit.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let edition_id = edition.id.clone();
    let update: EditionUpdate =
        serde_json::from_value(serde_json::json!({"language":"de","region":null})).unwrap();
    let edition = repository
        .update_edition(&edition_id, update)
        .await
        .unwrap();
    assert_eq!(edition.publication_id, publication.id);
    assert_eq!(edition.publisher.as_deref(), Some("Press"));
    let update: UnitUpdate = serde_json::from_value(
        serde_json::json!({"label":"2","kind":"volume","sort_key":null,"date":"2024-02-29"}),
    )
    .unwrap();
    let unit = repository.update_unit(&unit.id, update).await.unwrap();
    assert_eq!(unit.edition_id, edition.id);
    assert_eq!(unit.date_precision, Some(DatePrecision::Day));
    let invalid: UnitUpdate =
        serde_json::from_value(serde_json::json!({"date":"2024-02-30"})).unwrap();
    assert!(matches!(
        repository.update_unit(&unit.id, invalid).await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(
        repository.list_units(&edition.id).await.unwrap()[0]
            .date
            .as_deref(),
        Some("2024-02-29")
    );
    let clear: UnitUpdate = serde_json::from_value(serde_json::json!({"date":null})).unwrap();
    assert_eq!(
        repository
            .update_unit(&unit.id, clear)
            .await
            .unwrap()
            .date_precision,
        None
    );
    assert!(serde_json::from_value::<EditionUpdate>(serde_json::json!({"language":null})).is_err());
    assert!(serde_json::from_value::<UnitUpdate>(serde_json::json!({"kind":null})).is_err());
    assert!(matches!(
        repository
            .update_edition(
                "missing",
                serde_json::from_value(serde_json::json!({})).unwrap()
            )
            .await,
        Err(CatalogError::NotFound)
    ));
    let coverage: (String, String, String, i64) = sqlx::query_as("SELECT c.unit_id,c.evidence,f.signature,f.size_bytes FROM file_coverage c JOIN library_files f ON f.id=c.library_file_id WHERE f.id='file'")
        .fetch_one(store.reader()).await.unwrap();
    assert_eq!(
        coverage,
        (
            unit.id.clone(),
            "user_confirmed".into(),
            "signature".into(),
            42
        )
    );
    let targets: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT edition_id,unit_id FROM provider_links WHERE external_id='edition-1'",
    )
    .fetch_one(store.reader())
    .await
    .unwrap();
    assert_eq!(targets, (Some(edition.id.clone()), None));
    assert!(matches!(
        repository
            .create_provider_link(NewProviderLink {
                provider: "source".into(),
                external_id: "unit-1".into(),
                publication_id: None,
                edition_id: None,
                unit_id: Some(unit.id)
            })
            .await,
        Err(CatalogError::Conflict)
    ));
}

#[tokio::test]
async fn concurrent_manual_edit_and_refresh_preserve_the_manual_title() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Original", None))
        .await
        .unwrap();
    let refresh = repository.clone();
    let manual = repository.update_publication(
        &publication.id,
        PublicationUpdate {
            title: Some("Manual".into()),
            sort_title: None,
            run_label: None,
            known_unit_count: None,
        },
    );
    let provider = refresh.refresh_title(&publication.id, "Provider");
    let (manual, provider) = tokio::join!(manual, provider);
    manual.unwrap();
    provider.unwrap();
    let final_publication = repository
        .get_publication(&publication.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_publication.title, "Manual");
    assert!(final_publication.title_locked);
}

#[tokio::test]
async fn missing_publications_return_not_found_and_explicit_same_title_locks() {
    let (_directory, repository) = repository().await;
    assert!(matches!(
        repository
            .update_publication(
                "missing",
                PublicationUpdate {
                    title: None,
                    sort_title: None,
                    run_label: None,
                    known_unit_count: None
                }
            )
            .await,
        Err(CatalogError::NotFound)
    ));
    let publication = repository
        .create_publication(publication("Same", None))
        .await
        .unwrap();
    let changed = repository
        .update_publication(
            &publication.id,
            PublicationUpdate {
                title: Some("Same".into()),
                sort_title: None,
                run_label: None,
                known_unit_count: None,
            },
        )
        .await
        .unwrap();
    assert!(changed.title_locked);
}

#[tokio::test]
async fn cursor_is_filter_scoped_bounded_and_has_no_duplicates() {
    let (_directory, repository) = repository().await;
    for title in ["10th", "Alpha", "Beta"] {
        repository
            .create_publication(publication(title, None))
            .await
            .unwrap();
    }
    let first = repository
        .list_publications(1, None, Some(ContentType::Comic))
        .await
        .unwrap();
    let second = repository
        .list_publications(1, first.next_cursor.as_deref(), Some(ContentType::Comic))
        .await
        .unwrap();
    assert_ne!(first.items[0].id, second.items[0].id);
    assert!(matches!(
        repository.list_publications(0, None, None).await,
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        repository.list_publications(101, None, None).await,
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        repository
            .list_publications(1, first.next_cursor.as_deref(), Some(ContentType::Manga))
            .await,
        Err(CatalogError::Invalid(_))
    ));
}

#[tokio::test]
async fn provider_links_are_scoped_and_unique() {
    let (_directory, repository) = repository().await;
    let first_publication = repository
        .create_publication(publication("Link", None))
        .await
        .unwrap();
    let second_publication = repository
        .create_publication(publication("Other Link", None))
        .await
        .unwrap();
    repository
        .create_provider_link(NewProviderLink {
            provider: "comicvine".into(),
            external_id: "123".into(),
            publication_id: Some(first_publication.id.clone()),
            edition_id: None,
            unit_id: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        repository
            .create_provider_link(NewProviderLink {
                provider: "comicvine".into(),
                external_id: "123".into(),
                publication_id: Some(first_publication.id),
                edition_id: None,
                unit_id: None
            })
            .await,
        Err(CatalogError::Conflict)
    ));
    assert!(matches!(
        repository
            .create_provider_link(NewProviderLink {
                provider: "comicvine".into(),
                external_id: "123".into(),
                publication_id: Some(second_publication.id.clone()),
                edition_id: None,
                unit_id: None
            })
            .await,
        Err(CatalogError::Conflict)
    ));
    let edition = repository
        .create_edition(NewEdition {
            publication_id: second_publication.id,
            language: "en".into(),
            region: None,
            publisher: None,
        })
        .await
        .unwrap();
    let unit = repository
        .create_unit(NewUnit {
            edition_id: edition.id,
            label: "1".into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    repository
        .create_provider_link(NewProviderLink {
            provider: "comicvine".into(),
            external_id: "123".into(),
            publication_id: None,
            edition_id: None,
            unit_id: Some(unit.id),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn edition_and_unit_pages_are_bounded_and_scope_cursors() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Paged", None))
        .await
        .unwrap();
    let first = repository
        .create_edition(NewEdition {
            publication_id: publication.id.clone(),
            language: "en".into(),
            region: Some("GB".into()),
            publisher: None,
        })
        .await
        .unwrap();
    let second = repository
        .create_edition(NewEdition {
            publication_id: publication.id.clone(),
            language: "en".into(),
            region: Some("US".into()),
            publisher: None,
        })
        .await
        .unwrap();
    let page = repository
        .list_editions_page(&publication.id, 1, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        repository
            .list_editions_page(&publication.id, 1, page.next_cursor.as_deref())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(matches!(
        repository
            .list_editions_page("other", 1, page.next_cursor.as_deref())
            .await,
        Err(CatalogError::Invalid(_))
    ));
    repository
        .create_unit(NewUnit {
            edition_id: first.id.clone(),
            label: "1".into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    repository
        .create_unit(NewUnit {
            edition_id: first.id.clone(),
            label: "2".into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    assert_eq!(
        repository
            .list_units_page(&first.id, 1, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_ne!(first.id, second.id);
}

#[tokio::test]
async fn edition_paging_keeps_null_and_empty_regions() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Nullable regions", None))
        .await
        .unwrap();
    let mut expected = Vec::new();
    for region in [None, Some(""), Some("GB")] {
        let edition = repository
            .create_edition(NewEdition {
                publication_id: publication.id.clone(),
                language: "en".into(),
                region: region.map(str::to_owned),
                publisher: None,
            })
            .await
            .unwrap();
        expected.push(edition.id);
    }
    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = repository
            .list_editions_page(&publication.id, 1, cursor.as_deref())
            .await
            .unwrap();
        seen.extend(page.items.into_iter().map(|edition| edition.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    expected.sort();
    seen.sort();
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn publication_search_is_literal_bounded_and_cursor_scoped() {
    let (_directory, repository) = repository().await;
    for (title, run) in [
        ("Same", Some("100%_\\ run")),
        ("Same", Some("second 100%_\\ run")),
        ("100xx run", None),
        ("Été", None),
    ] {
        repository
            .create_publication(publication(title, run))
            .await
            .unwrap();
    }
    repository
        .create_publication(NewPublication {
            content_type: ContentType::Manga,
            ..publication("Same", Some("100%_\\ run"))
        })
        .await
        .unwrap();
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = repository
            .list_publications_search(
                1,
                cursor.as_deref(),
                Some(ContentType::Comic),
                Some(" 100%_\\ RUN "),
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Same");
        ids.push(page.items[0].id.clone());
        if let Some(next) = &page.next_cursor {
            for q in [None, Some("other")] {
                assert!(matches!(
                    repository
                        .list_publications_search(1, Some(next), Some(ContentType::Comic), q,)
                        .await,
                    Err(CatalogError::Invalid(_))
                ));
            }
            assert!(matches!(
                repository
                    .list_publications_search(
                        1,
                        Some(next),
                        Some(ContentType::Manga),
                        Some("100%_\\ run"),
                    )
                    .await,
                Err(CatalogError::Invalid(_))
            ));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(ids.len() < 3);
    }
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        repository
            .list_publications_search(100, None, None, Some("sAmE"))
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    assert_eq!(
        repository
            .list_publications_search(100, None, None, Some("Été"))
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(
        repository
            .list_publications_search(100, None, None, Some("missing"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for q in ["x".repeat(257), "é".repeat(129), "\n".into(), "a\0b".into()] {
        assert!(matches!(
            repository
                .list_publications_search(1, None, None, Some(&q))
                .await,
            Err(CatalogError::Invalid(_))
        ));
    }
    assert!(
        repository
            .list_publications_search(1, None, None, Some(&"x".repeat(256)))
            .await
            .is_ok()
    );
    let unfiltered = repository.list_publications(1, None, None).await.unwrap();
    let encoded = unfiltered.next_cursor.unwrap();
    // Old cursors lack q; their unfiltered meaning remains valid.
    use base64::Engine;
    let mut legacy: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&encoded)
            .unwrap(),
    )
    .unwrap();
    legacy.as_object_mut().unwrap().remove("q");
    let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&legacy).unwrap());
    let page = repository
        .list_publications_search(100, Some(&legacy), None, Some("   "))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert!(matches!(
        repository
            .list_publications_search(1, Some(&legacy), None, Some("same"))
            .await,
        Err(CatalogError::Invalid(_))
    ));
}

#[tokio::test]
async fn edition_and_unit_search_preserve_parent_scope_and_tied_sort_paging() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Lookup", None))
        .await
        .unwrap();
    let mut editions = Vec::new();
    for region in [None, Some(""), Some("GB")] {
        editions.push(
            repository
                .create_edition(NewEdition {
                    publication_id: publication.id.clone(),
                    language: "en".into(),
                    region: region.map(str::to_owned),
                    publisher: Some("Press%_\\".into()),
                })
                .await
                .unwrap(),
        );
    }
    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = repository
            .list_editions_page_search(&publication.id, 1, cursor.as_deref(), Some("press%_\\"))
            .await
            .unwrap();
        seen.push(page.items[0].id.clone());
        if let Some(next) = &page.next_cursor {
            assert!(matches!(
                repository
                    .list_editions_page_search(&publication.id, 1, Some(next), Some("en"))
                    .await,
                Err(CatalogError::Invalid(_))
            ));
            assert!(matches!(
                repository
                    .list_editions_page_search("other", 1, Some(next), Some("press%_\\"))
                    .await,
                Err(CatalogError::Invalid(_))
            ));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(seen.len() < 4);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3);
    for (q, count) in [("EN", 3), ("gb", 1), ("missing", 0)] {
        assert_eq!(
            repository
                .list_editions_page_search(&publication.id, 100, None, Some(q))
                .await
                .unwrap()
                .items
                .len(),
            count
        );
    }
    for edition in &editions {
        for label in ["12.5%_\\", "Annual 12.5%_\\", "12x5xxx"] {
            repository
                .create_unit(NewUnit {
                    edition_id: edition.id.clone(),
                    label: label.into(),
                    kind: UnitKind::Issue,
                    sort_key: Some("same".into()),
                    date: None,
                })
                .await
                .unwrap();
        }
    }
    let first = repository
        .list_units_page_search(&editions[0].id, 1, None, Some("12.5%_\\"))
        .await
        .unwrap();
    let next = first.next_cursor.as_deref().unwrap();
    let second = repository
        .list_units_page_search(&editions[0].id, 1, Some(next), Some("12.5%_\\"))
        .await
        .unwrap();
    assert_ne!(first.items[0].id, second.items[0].id);
    assert!(second.next_cursor.is_none());
    assert_eq!(second.items[0].edition_id, editions[0].id);
    for (id, q) in [(&editions[1].id, Some("12.5%_\\")), (&editions[0].id, None)] {
        assert!(matches!(
            repository
                .list_units_page_search(id, 1, Some(next), q)
                .await,
            Err(CatalogError::Invalid(_))
        ));
    }
    assert!(
        repository
            .list_units_page_search(&editions[0].id, 100, None, Some("missing"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(matches!(
        repository
            .list_units_page_search(&editions[0].id, 1, None, Some("a\tb"))
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        repository
            .list_editions_page_search(&publication.id, 1, None, Some(&"é".repeat(129)))
            .await,
        Err(CatalogError::Invalid(_))
    ));
}

async fn repository_with_store() -> (tempfile::TempDir, SqliteStore, CatalogRepository) {
    let directory = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let store = SqliteStore::open(directory.path()).await.unwrap();
    let repository = CatalogRepository::new(store.clone());
    (directory, store, repository)
}

async fn collect_pages(repository: &CatalogRepository, filter: PublicationFilter) -> Vec<String> {
    let mut ids = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = repository
            .list_publications_filtered(1, cursor.as_deref(), filter.clone())
            .await
            .unwrap();
        assert!(page.items.len() <= 1);
        ids.extend(page.items.into_iter().map(|item| item.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return ids,
        }
    }
}

#[tokio::test]
async fn publication_sort_and_availability_page_with_sort_bound_cursors() {
    let (_directory, store, repository) = repository_with_store().await;
    let mut created = Vec::new();
    for (title, created_at, has_file) in [
        ("Charlie", 300, true),
        ("alpha", 100, false),
        ("Bravo", 300, false),
        ("Delta", 200, true),
    ] {
        let publication = repository
            .create_publication(publication(title, None))
            .await
            .unwrap();
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("UPDATE publications SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(&publication.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        if has_file {
            let edition = repository
                .create_edition(NewEdition {
                    publication_id: publication.id.clone(),
                    language: "en".into(),
                    region: None,
                    publisher: None,
                })
                .await
                .unwrap();
            let unit = repository
                .create_unit(NewUnit {
                    edition_id: edition.id,
                    label: "1".into(),
                    kind: UnitKind::Issue,
                    sort_key: None,
                    date: None,
                })
                .await
                .unwrap();
            let file = uuid::Uuid::new_v4().to_string();
            let mut tx = store.begin_write().await.unwrap();
            sqlx::query("INSERT INTO library_files (id, path, format, signature, size_bytes) VALUES (?, ?, 'cbz', 'sig', 1)")
                .bind(&file).bind(format!("/fixture/{file}.cbz")).execute(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO file_coverage (library_file_id, unit_id, evidence) VALUES (?, ?, 'user_confirmed')")
                .bind(&file).bind(&unit.id).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
        created.push((publication.id, publication.sort_title, created_at, has_file));
    }
    let expected = |filter: &dyn Fn(bool) -> bool, sort: PublicationSort| {
        let mut rows: Vec<_> = created
            .iter()
            .filter(|row| filter(row.3))
            .cloned()
            .collect();
        match sort {
            PublicationSort::Title => rows.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0))),
            PublicationSort::RecentlyAdded => {
                rows.sort_by(|a, b| (b.2, &b.0).cmp(&(a.2, &a.0)));
            }
        }
        rows.into_iter().map(|row| row.0).collect::<Vec<_>>()
    };
    for sort in [PublicationSort::Title, PublicationSort::RecentlyAdded] {
        for (availability, keep) in [
            (Availability::All, &(|_| true) as &dyn Fn(bool) -> bool),
            (Availability::HasFiles, &|has| has),
            (Availability::NoFiles, &|has| !has),
        ] {
            let filter = PublicationFilter {
                sort,
                availability,
                ..PublicationFilter::default()
            };
            assert_eq!(
                collect_pages(&repository, filter).await,
                expected(keep, sort),
                "{sort:?} {availability:?}"
            );
        }
    }
    assert_eq!(
        collect_pages(&repository, PublicationFilter::default()).await,
        repository
            .list_publications(100, None, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>()
    );
    let recent = repository
        .list_publications_filtered(
            1,
            None,
            PublicationFilter {
                sort: PublicationSort::RecentlyAdded,
                ..PublicationFilter::default()
            },
        )
        .await
        .unwrap()
        .next_cursor
        .unwrap();
    let title = repository
        .list_publications(1, None, None)
        .await
        .unwrap()
        .next_cursor
        .unwrap();
    for (cursor, filter) in [
        (&recent, PublicationFilter::default()),
        (
            &title,
            PublicationFilter {
                sort: PublicationSort::RecentlyAdded,
                ..PublicationFilter::default()
            },
        ),
        (
            &recent,
            PublicationFilter {
                sort: PublicationSort::RecentlyAdded,
                availability: Availability::HasFiles,
                ..PublicationFilter::default()
            },
        ),
    ] {
        assert!(matches!(
            repository
                .list_publications_filtered(1, Some(cursor), filter)
                .await,
            Err(CatalogError::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn unit_kind_filter_pages_and_binds_cursor() {
    let (_directory, repository) = repository().await;
    let publication = repository
        .create_publication(publication("Kinds", None))
        .await
        .unwrap();
    let edition = repository
        .create_edition(NewEdition {
            publication_id: publication.id,
            language: "en".into(),
            region: None,
            publisher: None,
        })
        .await
        .unwrap();
    for (label, kind) in [
        ("1", UnitKind::Issue),
        ("2", UnitKind::Volume),
        ("3", UnitKind::Issue),
        ("4", UnitKind::Special),
        ("5", UnitKind::Issue),
    ] {
        repository
            .create_unit(NewUnit {
                edition_id: edition.id.clone(),
                label: label.into(),
                kind,
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
    }
    let mut labels = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = repository
            .list_units_filtered(
                &edition.id,
                1,
                cursor.as_deref(),
                None,
                Some(UnitKind::Issue),
            )
            .await
            .unwrap();
        assert!(page.items.iter().all(|unit| unit.kind == UnitKind::Issue));
        labels.extend(page.items.into_iter().map(|unit| unit.label));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(labels, ["1", "3", "5"]);
    let first = repository
        .list_units_filtered(&edition.id, 1, None, None, Some(UnitKind::Issue))
        .await
        .unwrap()
        .next_cursor
        .unwrap();
    for kind in [None, Some(UnitKind::Volume)] {
        assert!(matches!(
            repository
                .list_units_filtered(&edition.id, 1, Some(&first), None, kind)
                .await,
            Err(CatalogError::Invalid(_))
        ));
    }
    assert!(
        repository
            .list_units_filtered(&edition.id, 100, None, None, Some(UnitKind::Chapter))
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
