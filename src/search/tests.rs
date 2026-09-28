use super::*;
use crate::{
    catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
    search::selection::{DecisionAction, NewDecision, SelectionRepository},
    settings::{EncryptionKey, IntegrationKind, Settings},
    store::sqlite::SqliteStore,
};

async fn catalog_unit(store: &SqliteStore, title: &str, content_type: ContentType) -> String {
    let catalog = CatalogRepository::new(store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type,
            title: title.into(),
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
    catalog
        .create_unit(NewUnit {
            edition_id: edition.id,
            label: "12.5".into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn opaque_handles_are_scoped_to_configured_source_and_owner() {
    let dir = mock::private_directory();
    let store = SqliteStore::open(dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(dir.path()).await.unwrap(),
    );
    let server = mock::Server::start(store.clone()).await;
    let first = mock::integration(&settings, &server, IntegrationKind::Prowlarr, 7, false).await;
    let second = mock::integration(&settings, &server, IntegrationKind::Prowlarr, 8, false).await;
    let search = Search::new(store.clone(), settings.clone());
    let request = ReleaseSearch {
        integration_id: first.clone(),
        content_type: ContentType::Comic,
        query: "Comic 12.5".into(),
        unit_id: None,
        offset: 0,
        limit: 20,
    };
    let a = search.releases("owner", request.clone()).await.unwrap();
    assert!(a.target.is_none());
    assert!(a.releases.iter().all(|release| release.evaluation.is_none()
        && release.assessment_id.is_none()
        && release.assessment_expires_at.is_none()));
    assert!(a.releases.iter().all(|release| !matches!(&release.evidence.title_stem, crate::providers::release_evidence::Evidence::Explicit(value) if value.contains("secret"))));
    let json = serde_json::to_string(&a).unwrap();
    for secret in [
        "guid-secret",
        "payload-secret",
        "fixture-private-key",
        "http:",
        "guid",
    ] {
        assert!(!json.contains(secret));
    }
    mock::allow_search(&store).await;
    let repeat = search.releases("owner", request.clone()).await.unwrap();
    assert_eq!(
        a.releases[0].release_handle,
        repeat.releases[0].release_handle
    );
    mock::allow_search(&store).await;
    let b = search
        .releases(
            "owner",
            ReleaseSearch {
                integration_id: second,
                ..request.clone()
            },
        )
        .await
        .unwrap();
    assert_ne!(a.releases[0].release_handle, b.releases[0].release_handle);
    mock::allow_search(&store).await;
    let other = search.releases("another-owner", request).await.unwrap();
    assert_ne!(
        a.releases[0].release_handle,
        other.releases[0].release_handle
    );
    mock::allow_search(&store).await;
    assert_eq!(
        selected_payload(&store, &settings, &a.releases[0].release_handle)
            .await
            .unwrap()
            .0,
        mock::NZB
    );
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .requests
            .last()
            .unwrap()
            .contains("/payload/7?")
    );
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT guid_digest, query FROM search_releases")
            .fetch_all(store.reader())
            .await
            .unwrap();
    assert!(
        rows.iter()
            .all(|(guid, query)| guid.len() == 64 && !query.contains("secret"))
    );
}

#[tokio::test]
async fn cooldown_is_atomic_across_connections_and_survives_reopen() {
    let dir = mock::private_directory();
    let a = SqliteStore::open(dir.path()).await.unwrap();
    let b = SqliteStore::open(dir.path()).await.unwrap();
    let (first, second) = tokio::join!(
        reserve_cooldown(&a, "shared-credential", 100),
        reserve_cooldown(&b, "shared-credential", 100)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    a.close().await;
    b.close().await;
    let reopened = SqliteStore::open(dir.path()).await.unwrap();
    assert!(matches!(
        reserve_cooldown(&reopened, "shared-credential", 100).await,
        Err(SearchError::Cooldown { .. })
    ));
}

#[tokio::test]
async fn unsafe_release_titles_are_redacted_before_serialization() {
    let dir = mock::private_directory();
    let store = SqliteStore::open(dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(dir.path()).await.unwrap(),
    );
    let server = mock::Server::start(store.clone()).await;
    let integration_id =
        mock::integration(&settings, &server, IntegrationKind::Prowlarr, 7, false).await;
    let search = Search::new(store, settings);
    let page = search
        .releases(
            "owner",
            ReleaseSearch {
                integration_id,
                content_type: ContentType::Comic,
                query: "Comic token=release-secret".into(),
                unit_id: None,
                offset: 0,
                limit: 20,
            },
        )
        .await
        .unwrap();
    let json = serde_json::to_string(&page).unwrap();
    assert!(!json.contains("release-secret"));
    assert!(json.contains("Release title unavailable"));
}

#[tokio::test]
async fn target_failures_happen_before_provider_io() {
    let dir = mock::private_directory();
    let store = SqliteStore::open(dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(dir.path()).await.unwrap(),
    );
    let server = mock::Server::start(store.clone()).await;
    let integration =
        mock::integration(&settings, &server, IntegrationKind::Prowlarr, 7, false).await;
    let search = Search::new(store.clone(), settings);
    let request = ReleaseSearch {
        integration_id: integration,
        content_type: ContentType::Comic,
        query: "Comic 12.5".into(),
        unit_id: Some("missing".into()),
        offset: 0,
        limit: 20,
    };
    assert!(matches!(
        search.releases("owner", request.clone()).await,
        Err(SearchError::UnitNotFound)
    ));
    let unit_id = catalog_unit(&store, "Magazine", ContentType::Magazine).await;
    assert!(matches!(
        search
            .releases(
                "owner",
                ReleaseSearch {
                    unit_id: Some(unit_id),
                    ..request
                }
            )
            .await,
        Err(SearchError::TargetChanged)
    ));
    assert!(server.seen.lock().unwrap().requests.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM search_cooldowns")
            .fetch_one(store.reader())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn target_assessment_is_fresh_while_handle_identity_is_unchanged() {
    let dir = mock::private_directory();
    let store = SqliteStore::open(dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(dir.path()).await.unwrap(),
    );
    let server = mock::Server::start(store.clone()).await;
    let integration =
        mock::integration(&settings, &server, IntegrationKind::Prowlarr, 7, false).await;
    let first_unit = catalog_unit(&store, "Comic", ContentType::Comic).await;
    let second_unit = catalog_unit(&store, "Second", ContentType::Comic).await;
    let search = Search::new(store.clone(), settings);
    let request = ReleaseSearch {
        integration_id: integration,
        content_type: ContentType::Comic,
        query: "Comic #12.5 [language:en] [cbz]".into(),
        unit_id: Some(first_unit.clone()),
        offset: 0,
        limit: 20,
    };
    let first = search.releases("owner", request.clone()).await.unwrap();
    let provider_requests = server.seen.lock().unwrap().requests.len();
    let cached = search
        .assess_release(
            "owner",
            ReleaseAssessmentRequest {
                release_handle: first.releases[0].release_handle.clone(),
                unit_id: first_unit.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cached.assessment_id,
        *first.releases[0].assessment_id.as_ref().unwrap()
    );
    assert_eq!(cached.target.unit.id, first_unit);
    assert!(cached.active_rejection.is_none());
    assert_eq!(
        server.seen.lock().unwrap().requests.len(),
        provider_requests
    );
    let rejection = SelectionRepository::new(store.clone())
        .decide(
            "owner",
            "cached-rejection",
            NewDecision {
                assessment_id: cached.assessment_id.clone(),
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: Some("not this release".into()),
            },
        )
        .await
        .unwrap();
    let reassessed = search
        .assess_release(
            "owner",
            ReleaseAssessmentRequest {
                release_handle: first.releases[0].release_handle.clone(),
                unit_id: first_unit.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(reassessed.assessment_id, cached.assessment_id);
    assert_eq!(reassessed.active_rejection.unwrap().id, rejection.id);
    assert_eq!(
        server.seen.lock().unwrap().requests.len(),
        provider_requests
    );
    mock::allow_search(&store).await;
    let second = search
        .releases(
            "owner",
            ReleaseSearch {
                unit_id: Some(second_unit.clone()),
                ..request.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        first.releases[0].release_handle,
        second.releases[0].release_handle
    );
    assert_eq!(first.target.as_ref().unwrap().unit.id, first_unit);
    assert_eq!(second.target.as_ref().unwrap().unit.id, second_unit);
    let first_id = first.releases[0].assessment_id.as_ref().unwrap();
    let second_id = second.releases[0].assessment_id.as_ref().unwrap();
    assert_ne!(first_id, second_id);
    let saved: (String, String, i64) =
        sqlx::query_as("SELECT owner, unit_id, expires_at FROM release_assessments WHERE id = ?")
            .bind(first_id)
            .fetch_one(store.reader())
            .await
            .unwrap();
    assert_eq!(saved.0, "owner");
    assert_eq!(saved.1, first_unit);
    assert_eq!(Some(saved.2), first.releases[0].assessment_expires_at);
    mock::allow_search(&store).await;
    let repeated = search.releases("owner", request).await.unwrap();
    assert_eq!(repeated.releases[0].assessment_id.as_ref(), Some(first_id));
    assert_eq!(
        repeated.releases[0]
            .active_rejection
            .as_ref()
            .map(|decision| decision.id.as_str()),
        Some(rejection.id.as_str())
    );
    assert_eq!(
        first.releases[0].evaluation.as_ref().unwrap().eligibility,
        crate::search::matching::Eligibility::Unknown
    );
    assert_eq!(
        first.releases[0].evaluation.as_ref().unwrap().reasons,
        vec![crate::search::matching::MatchReason {
            field: crate::search::matching::MatchField::ContentType,
            code: crate::search::matching::ReasonCode::Missing,
        }]
    );
    assert_eq!(
        second.releases[0].evaluation.as_ref().unwrap().eligibility,
        crate::search::matching::Eligibility::Ineligible
    );
    assert!(
        second.releases[0]
            .evaluation
            .as_ref()
            .unwrap()
            .reasons
            .contains(&crate::search::matching::MatchReason {
                field: crate::search::matching::MatchField::Title,
                code: crate::search::matching::ReasonCode::Conflict,
            })
    );
}

#[test]
fn selection_failures_map_to_specific_search_errors() {
    use super::service::{assessment_error, selection_error};
    use crate::search::selection::SelectionError;
    assert!(matches!(
        selection_error(SelectionError::Invalid),
        SearchError::Invalid
    ));
    assert!(matches!(
        selection_error(SelectionError::NotFound),
        SearchError::UnitNotFound
    ));
    assert!(matches!(
        selection_error(SelectionError::Expired),
        SearchError::NotFound
    ));
    assert!(matches!(
        selection_error(SelectionError::Database),
        SearchError::Database
    ));
    // An assessment window that closed between the SQL check and recording is an expired handle.
    assert!(matches!(
        assessment_error(SelectionError::Invalid, now()),
        SearchError::NotFound
    ));
    assert!(matches!(
        assessment_error(SelectionError::Invalid, now() + 3600),
        SearchError::Invalid
    ));
}

#[test]
fn rejected_selection_maps_to_release_rejected_conflict() {
    use crate::{httpapi::errors::ApiError, search::selection::SelectionError};
    for error in [SelectionError::Rejected, SelectionError::Superseded] {
        let error = super::service::selection_error(error);
        assert!(matches!(error, SearchError::ReleaseRejected));
        let api = ApiError::from(error);
        assert_eq!(api.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(api.code, "release_rejected");
    }
}
