use super::*;
use crate::{
    catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
    library::roots::Library,
    search::{ReleaseSearch, Search, mock},
    search::{
        matching::Eligibility,
        selection::{DecisionAction, NewDecision, SelectionRepository},
    },
    settings::EncryptionKey,
};
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio::sync::Notify;

struct Fixture {
    dir: tempfile::TempDir,
    source: tempfile::TempDir,
    destination: tempfile::TempDir,
    store: SqliteStore,
    settings: Settings,
    pipeline: Pipeline,
    server: mock::Server,
    request: AcquisitionRequest,
    source_id: String,
    destination_id: String,
}
impl Fixture {
    async fn new(content_type: ContentType, torrent: bool) -> Self {
        let dir = mock::private_directory();
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(dir.path()).await.unwrap(),
        );
        let server = mock::Server::start(store.clone()).await;
        let integration_id =
            mock::integration(&settings, &server, IntegrationKind::Prowlarr, 7, torrent).await;
        let client_id = mock::integration(
            &settings,
            &server,
            if torrent {
                IntegrationKind::QBittorrent
            } else {
                IntegrationKind::Sabnzbd
            },
            7,
            torrent,
        )
        .await;
        let library = Library::new(store.clone());
        let source_id = library
            .register_root("download", source.path())
            .await
            .unwrap()
            .id;
        let destination_id = library
            .register_root("library", destination.path())
            .await
            .unwrap()
            .id;
        let catalog = CatalogRepository::new(store.clone());
        let publication = catalog
            .create_publication(NewPublication {
                content_type: content_type.clone(),
                title: "Owned Fixture".into(),
                sort_title: None,
                run_label: Some("2026".into()),
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
        let unit = catalog
            .create_unit(NewUnit {
                edition_id: edition.id,
                label: if content_type == ContentType::Magazine {
                    "7-8"
                } else {
                    "12.5"
                }
                .into(),
                kind: if content_type == ContentType::Magazine {
                    UnitKind::Combined
                } else {
                    UnitKind::Issue
                },
                sort_key: None,
                date: None,
            })
            .await
            .unwrap();
        let search = Search::new(store.clone(), settings.clone());
        let releases = search
            .releases(
                "owner",
                ReleaseSearch {
                    integration_id,
                    content_type,
                    query: "Owned Fixture".into(),
                    unit_id: Some(unit.id.clone()),
                    offset: 0,
                    limit: 20,
                },
            )
            .await
            .unwrap();
        let assessed = &releases.releases[0];
        let assessment_id = assessed.assessment_id.clone().unwrap();
        let decision = SelectionRepository::new(store.clone())
            .decide(
                "owner",
                "fixture-selection",
                NewDecision {
                    assessment_id: assessment_id.clone(),
                    action: DecisionAction::Selected,
                    acknowledged_assessment_id: (assessed.evaluation.as_ref().unwrap().eligibility
                        == Eligibility::Unknown)
                        .then_some(assessment_id),
                    reason: None,
                },
            )
            .await
            .unwrap();
        let request = AcquisitionRequest {
            release_handle: releases.releases[0].release_handle.clone(),
            client_id,
            unit_id: unit.id,
            selection_decision_id: Some(decision.id),
            destination: Some(DestinationSelection {
                root_id: destination_id.clone(),
                relative_path: "selected.cbz".into(),
            }),
        };
        let bytes = include_bytes!("../../tests/fixtures/natural-order.cbz");
        std::fs::write(source.path().join("owned.cbz"), bytes).unwrap();
        if torrent {
            let piece = Sha1::digest(bytes);
            let mut info = format!(
                "d6:lengthi{}e4:name9:owned.cbz12:piece lengthi16384e6:pieces20:",
                bytes.len()
            )
            .into_bytes();
            info.extend_from_slice(&piece);
            info.push(b'e');
            let mut torrent = b"d4:info".to_vec();
            torrent.extend_from_slice(&info);
            torrent.push(b'e');
            let hash = search::torrent::v1_infohash(&torrent).unwrap();
            let mut seen = server.seen.lock().unwrap();
            seen.torrent = torrent;
            seen.expected_hash = hash;
        }
        let pipeline = Pipeline::new(store.clone());
        Self {
            dir,
            source,
            destination,
            store,
            settings,
            pipeline,
            server,
            request,
            source_id,
            destination_id,
        }
    }
    async fn create(&self, key: &str) -> Acquisition {
        self.pipeline
            .create(&self.settings, "owner", key, self.request.clone())
            .await
            .unwrap()
    }
    async fn submit(&self, id: &str) {
        self.server.seen.lock().unwrap().own_tag = format!("libraryd-{id}");
        mock::allow_search(&self.store).await;
        self.pipeline
            .tick_job(self.settings.clone(), id)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn lost_response_is_fenced_across_reopen_and_generic_retry() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("lost-response").await;
    f.server.seen.lock().unwrap().drop_response = true;
    f.submit(&intent.id).await;
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(view.reason, Some(AcquisitionReason::UncertainSubmission));
    assert!(view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
    f.store.close().await;
    let store = SqliteStore::open(f.dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
    );
    let pipeline = Pipeline::new(store.clone());
    assert!(matches!(
        crate::jobs::Jobs::new(store.clone())
            .retry_reviewed(&intent.job_id)
            .await,
        Err(crate::jobs::JobError::Conflict)
    ));
    pipeline
        .tick_job(settings.clone(), &intent.id)
        .await
        .unwrap();
    pipeline.tick(settings.clone()).await.unwrap();
    let duplicate = pipeline
        .create(&settings, "owner", "lost-response", f.request.clone())
        .await
        .unwrap();
    assert_eq!(duplicate.id, intent.id);
    assert_eq!(duplicate.state, AcquisitionState::NeedsReview);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn duplicate_intent_key_and_target_conflicts_are_atomic() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let (a, b) = tokio::join!(f.create("same-key"), f.create("same-key"));
    assert_eq!(a.id, b.id);
    let mut changed = f.request.clone();
    changed.destination.as_mut().unwrap().relative_path = "another.cbz".into();
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "same-key", changed)
            .await,
        Err(PipelineError::Conflict)
    ));
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "new-key", f.request.clone())
            .await,
        Err(PipelineError::Conflict)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisition_intents")
        .fetch_one(f.store.reader())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
    assert!(matches!(
        f.pipeline.get("someone-else", &a.id).await,
        Err(PipelineError::NotFound)
    ));
}

#[tokio::test]
async fn new_intents_require_a_current_selected_decision_without_active_rejection() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let mut missing = f.request.clone();
    missing.selection_decision_id = None;
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "missing-selection", missing)
            .await,
        Err(PipelineError::Invalid)
    ));
    let decision_id = f.request.selection_decision_id.as_ref().unwrap();
    let assessment_id: String =
        sqlx::query_scalar("SELECT assessment_id FROM release_decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    SelectionRepository::new(f.store.clone())
        .decide(
            "owner",
            "reject-before-acquisition",
            NewDecision {
                assessment_id,
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: Some("wrong release".into()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "actively-rejected", f.request.clone())
            .await,
        Err(PipelineError::Selection(SelectionError::Rejected))
    ));
    let intents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisition_intents")
        .fetch_one(f.store.reader())
        .await
        .unwrap();
    assert_eq!(intents, 0);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

async fn reject_and_revoke(f: &Fixture, key: &str) -> String {
    let decision_id = f.request.selection_decision_id.as_ref().unwrap();
    let assessment_id: String =
        sqlx::query_scalar("SELECT assessment_id FROM release_decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    let repository = SelectionRepository::new(f.store.clone());
    let rejection = repository
        .decide(
            "owner",
            &format!("{key}-reject"),
            NewDecision {
                assessment_id: assessment_id.clone(),
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: None,
            },
        )
        .await
        .unwrap();
    repository
        .revoke_rejection("owner", &format!("{key}-revoke"), &rejection.id)
        .await
        .unwrap();
    assessment_id
}

#[tokio::test]
async fn revoked_rejection_does_not_revive_an_earlier_selection_at_create() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let assessment_id = reject_and_revoke(&f, "create").await;
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "stale-selection", f.request.clone())
            .await,
        Err(PipelineError::Selection(SelectionError::Superseded))
    ));
    let intents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisition_intents")
        .fetch_one(f.store.reader())
        .await
        .unwrap();
    assert_eq!(intents, 0);
    let acknowledged: Option<String> =
        sqlx::query_scalar("SELECT acknowledged_assessment_id FROM release_decisions WHERE id = ?")
            .bind(f.request.selection_decision_id.as_ref().unwrap())
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    let fresh = SelectionRepository::new(f.store.clone())
        .decide(
            "owner",
            "select-after-revocation",
            NewDecision {
                assessment_id,
                action: DecisionAction::Selected,
                acknowledged_assessment_id: acknowledged,
                reason: None,
            },
        )
        .await
        .unwrap();
    let mut request = f.request.clone();
    request.selection_decision_id = Some(fresh.id);
    let intent = f
        .pipeline
        .create(&f.settings, "owner", "fresh-selection", request)
        .await
        .unwrap();
    assert_eq!(intent.state, AcquisitionState::Queued);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn revoked_rejection_does_not_revive_a_queued_selection_at_submission() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("queued-before-rejection").await;
    reject_and_revoke(&f, "queued").await;
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(
        view.reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn queued_legacy_intent_without_decision_requires_review_before_provider_or_client_io() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("legacy-queued").await;
    let requests_before = f.server.seen.lock().unwrap().requests.len();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET selection_decision_id = NULL WHERE id = ?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(
        view.reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert!(!view.submitted);
    let seen = f.server.seen.lock().unwrap();
    assert_eq!(seen.requests.len(), requests_before);
    assert_eq!(seen.submissions, 0);
}

#[tokio::test]
async fn queued_selection_invalidated_by_rejection_never_reaches_the_client() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("rejected-while-queued").await;
    let decision_id = f.request.selection_decision_id.as_ref().unwrap();
    let assessment_id: String =
        sqlx::query_scalar("SELECT assessment_id FROM release_decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    SelectionRepository::new(f.store.clone())
        .decide(
            "owner",
            "reject-queued",
            NewDecision {
                assessment_id,
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: None,
            },
        )
        .await
        .unwrap();
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(
        view.reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn source_configuration_change_is_denied_at_create_and_queued_fence() {
    let before_create = Fixture::new(ContentType::Comic, false).await;
    let source_id: String =
        sqlx::query_scalar("SELECT integration_id FROM search_releases WHERE handle = ?")
            .bind(&before_create.request.release_handle)
            .fetch_one(before_create.store.reader())
            .await
            .unwrap();
    before_create
        .settings
        .update(
            &source_id,
            crate::settings::UpdateIntegration {
                base_url: Some("http://127.0.0.1:1/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        before_create
            .pipeline
            .create(
                &before_create.settings,
                "owner",
                "changed-source",
                before_create.request.clone(),
            )
            .await,
        Err(PipelineError::Selection(SelectionError::Changed))
    ));

    let queued = Fixture::new(ContentType::Comic, false).await;
    let intent = queued.create("queued-source-change").await;
    let source_id: String =
        sqlx::query_scalar("SELECT integration_id FROM search_releases WHERE handle = ?")
            .bind(&queued.request.release_handle)
            .fetch_one(queued.store.reader())
            .await
            .unwrap();
    queued
        .settings
        .update(
            &source_id,
            crate::settings::UpdateIntegration {
                base_url: Some("http://127.0.0.1:1/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    queued
        .pipeline
        .tick_job(queued.settings.clone(), &intent.id)
        .await
        .unwrap();
    let view = queued.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(
        view.reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert!(!view.submitted);
    assert_eq!(queued.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn fresh_provider_evidence_change_is_stopped_at_submission_fence() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("fresh-evidence-change").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE search_releases SET query = 'Different Publication #99 [language:fr] [pdf]' WHERE handle = ?")
        .bind(&f.request.release_handle)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    mock::allow_search(&f.store).await;
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(
        view.reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn cancellation_during_fetch_wins_over_selection_invalidation() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("cancel-during-fetch").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
    }
    mock::allow_search(&f.store).await;
    let started_wait = started.notified();
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let acquisition_id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &acquisition_id).await });
    started_wait.await;
    let decision_id = f.request.selection_decision_id.as_ref().unwrap();
    let assessment_id: String =
        sqlx::query_scalar("SELECT assessment_id FROM release_decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    SelectionRepository::new(f.store.clone())
        .decide(
            "owner",
            "reject-during-fetch",
            NewDecision {
                assessment_id,
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: None,
            },
        )
        .await
        .unwrap();
    crate::jobs::Jobs::new(f.store.clone())
        .request_cancel(&intent.job_id)
        .await
        .unwrap();
    release.notify_one();
    worker.await.unwrap().unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::Canceled);
    assert_eq!(view.reason, None);
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn client_configuration_change_during_fetch_stops_submission() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("client-change-during-fetch").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
    }
    mock::allow_search(&f.store).await;
    let started_wait = started.notified();
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let acquisition_id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &acquisition_id).await });
    started_wait.await;
    f.settings
        .update(
            &f.request.client_id,
            crate::settings::UpdateIntegration {
                base_url: Some("http://127.0.0.1:1/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    release.notify_one();
    worker.await.unwrap().unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(view.reason, Some(AcquisitionReason::ConfigurationChanged));
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn cancellation_during_fetch_wins_over_failed_payload() {
    let f = Fixture::new(ContentType::Manga, true).await;
    let intent = f.create("cancel-during-failed-fetch").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
    }
    mock::allow_search(&f.store).await;
    let started_wait = started.notified();
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let acquisition_id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &acquisition_id).await });
    started_wait.await;
    f.server.seen.lock().unwrap().torrent = b"not a valid payload".to_vec();
    crate::jobs::Jobs::new(f.store.clone())
        .request_cancel(&intent.job_id)
        .await
        .unwrap();
    release.notify_one();
    worker.await.unwrap().unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::Canceled);
    assert_eq!(view.reason, None);
    assert!(!view.submitted);
    let job_state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
        .bind(&intent.job_id)
        .fetch_one(f.store.reader())
        .await
        .unwrap();
    assert_eq!(job_state, "canceled");
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn cleared_client_credentials_during_fetch_stop_submission() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("credentials-cleared-during-fetch").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
    }
    mock::allow_search(&f.store).await;
    let started_wait = started.notified();
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let acquisition_id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &acquisition_id).await });
    started_wait.await;
    f.settings
        .update(
            &f.request.client_id,
            crate::settings::UpdateIntegration {
                api_key: crate::settings::SecretUpdate::Clear,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    release.notify_one();
    worker.await.unwrap().unwrap();
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(view.reason, Some(AcquisitionReason::ConfigurationChanged));
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn fenced_torrent_hash_duplicate_is_not_an_uncertain_submission() {
    let f = Fixture::new(ContentType::Manga, true).await;
    let first = f.create("first-torrent").await;
    f.submit(&first.id).await;
    assert!(f.pipeline.get("owner", &first.id).await.unwrap().submitted);
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET state = 'canceled' WHERE id = ?")
        .bind(&first.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let second = f.create("second-torrent").await;
    f.submit(&second.id).await;
    let view = f.pipeline.get("owner", &second.id).await.unwrap();
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(view.reason, Some(AcquisitionReason::DuplicateTorrent));
    assert!(!view.submitted);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn identical_replay_survives_selection_and_handle_expiry() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("expiry-replay").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE release_assessments SET created_at = 0, expires_at = 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE search_releases SET expires_at = 0")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let replay = f
        .pipeline
        .create(&f.settings, "owner", "expiry-replay", f.request.clone())
        .await
        .unwrap();
    assert_eq!(replay.id, intent.id);
}

#[tokio::test]
async fn all_three_content_types_require_explicit_association_then_copy_to_journal_completion() {
    for (content, torrent) in [
        (ContentType::Comic, false),
        (ContentType::Manga, true),
        (ContentType::Magazine, false),
    ] {
        let f = Fixture::new(content, torrent).await;
        let intent = f.create("fixture-acquisition").await;
        f.submit(&intent.id).await;
        let accepted = f.pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(accepted.state, AcquisitionState::Downloading);
        assert_eq!(accepted.receipt_count, if torrent { 1 } else { 2 });
        let serialized = serde_json::to_string(&accepted).unwrap();
        for private in [
            "private",
            "http",
            "external_id",
            "category",
            "payload",
            "path",
            "SABnzbd",
            "libraryd-",
        ] {
            assert!(!serialized.contains(private));
        }
        f.server.seen.lock().unwrap().completed = true;
        for _ in 0..accepted.receipt_count {
            f.pipeline
                .tick_job(f.settings.clone(), &intent.id)
                .await
                .unwrap();
        }
        let downloaded = f.pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(downloaded.state, AcquisitionState::Downloaded);
        assert_eq!(
            downloaded.reason,
            Some(AcquisitionReason::FileAssociationRequired)
        );
        assert!(!f.destination.path().join("selected.cbz").exists());
        let coverage: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_coverage")
            .fetch_one(f.store.reader())
            .await
            .unwrap();
        assert_eq!(coverage, 0);
        let association = FileAssociation {
            source_root_id: f.source_id.clone(),
            source_relative_path: "owned.cbz".into(),
            destination: None,
        };
        f.pipeline
            .associate_file("owner", &intent.id, association)
            .await
            .unwrap();
        for _ in 0..10 {
            f.pipeline
                .tick_job(f.settings.clone(), &intent.id)
                .await
                .unwrap();
        }
        let final_view = f.pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(
            final_view.state,
            AcquisitionState::Completed,
            "{final_view:?}"
        );
        let phase: String = sqlx::query_scalar("SELECT phase FROM import_operations WHERE id = ?")
            .bind(final_view.import_id.as_ref().unwrap())
            .fetch_one(f.store.reader())
            .await
            .unwrap();
        assert_eq!(phase, "done");
        let original = std::fs::read(f.source.path().join("owned.cbz")).unwrap();
        assert_eq!(
            original,
            include_bytes!("../../tests/fixtures/natural-order.cbz")
        );
        assert_eq!(
            original,
            std::fs::read(f.destination.path().join("selected.cbz")).unwrap()
        );
        let units: Vec<String> = sqlx::query_scalar("SELECT unit_id FROM file_coverage")
            .fetch_all(f.store.reader())
            .await
            .unwrap();
        assert_eq!(units, vec![f.request.unit_id]);
    }
}

#[tokio::test]
async fn concurrent_submitters_share_durable_fence_and_store_every_receipt() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("two-workers").await;
    mock::allow_search(&f.store).await;
    let second = SqliteStore::open(f.dir.path()).await.unwrap();
    let second_pipeline = Pipeline::new(second);
    let (a, b) = tokio::join!(
        f.pipeline.tick_job(f.settings.clone(), &intent.id),
        second_pipeline.tick_job(f.settings.clone(), &intent.id)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
    assert_eq!(
        f.pipeline
            .get("owner", &intent.id)
            .await
            .unwrap()
            .receipt_count,
        2
    );
}

#[tokio::test]
async fn wrong_content_type_owner_and_changed_client_are_denied() {
    let f = Fixture::new(ContentType::Comic, false).await;
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "another-owner", "foreign", f.request.clone())
            .await,
        Err(PipelineError::NotFound)
    ));
    let intent = f.create("changed-client").await;
    f.settings
        .update(
            &f.request.client_id,
            crate::settings::UpdateIntegration {
                base_url: Some("http://127.0.0.1:1/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    assert_eq!(
        f.pipeline.get("owner", &intent.id).await.unwrap().reason,
        Some(AcquisitionReason::ConfigurationChanged)
    );
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn foreign_client_category_never_becomes_downloaded() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("foreign-category").await;
    f.submit(&intent.id).await;
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.category = Some("someone-else".into());
        seen.completed = true;
    }
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    assert_eq!(
        f.pipeline.get("owner", &intent.id).await.unwrap().state,
        AcquisitionState::NeedsReview
    );
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn receipt_storage_failure_rolls_back_all_receipts_and_preserves_fence() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("receipt-failure").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("CREATE TRIGGER reject_second_receipt BEFORE INSERT ON acquisition_receipts WHEN NEW.ordinal = 1 BEGIN SELECT RAISE(ABORT, 'fixture receipt failure'); END")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    mock::allow_search(&f.store).await;
    assert!(matches!(
        f.pipeline.tick_job(f.settings.clone(), &intent.id).await,
        Err(PipelineError::Database)
    ));
    let view = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert!(view.submitted);
    assert_eq!(view.state, AcquisitionState::NeedsReview);
    assert_eq!(view.receipt_count, 0);
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn worker_claim_excludes_other_instances_and_stale_claim_cannot_change_state() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("claim-fence").await;
    let old = WorkClaim {
        id: intent.id.clone(),
        token: Uuid::new_v4().to_string(),
    };
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query(
        "UPDATE acquisition_runs SET work_token = ?, work_until = unixepoch() + 300 WHERE id = ?",
    )
    .bind(&old.token)
    .bind(&old.id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let other_store = SqliteStore::open(f.dir.path()).await.unwrap();
    let other = Pipeline::new(other_store);
    mock::allow_search(&f.store).await;
    other
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET work_until = 0 WHERE id = ?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    other
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    f.pipeline
        .set_state(&old, "needs_review", Some("source_unavailable"))
        .await
        .unwrap();
    assert_eq!(
        f.pipeline.get("owner", &intent.id).await.unwrap().state,
        AcquisitionState::Downloading
    );
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn scheduled_tick_throttles_receipts_and_cannot_auto_import() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("poll-spacing").await;
    f.submit(&intent.id).await;
    assert!(f.pipeline.tick(f.settings.clone()).await.unwrap());
    let count = f.server.seen.lock().unwrap().requests.len();
    assert!(!f.pipeline.tick(f.settings.clone()).await.unwrap());
    assert_eq!(f.server.seen.lock().unwrap().requests.len(), count);
    assert_eq!(
        f.pipeline.get("owner", &intent.id).await.unwrap().state,
        AcquisitionState::Downloading
    );
}

#[tokio::test]
async fn wrong_content_type_and_destination_reassociation_are_rejected() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE publications SET content_type = 'manga'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        f.pipeline
            .create(&f.settings, "owner", "wrong-type", f.request.clone())
            .await,
        Err(PipelineError::Invalid)
    ));
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE publications SET content_type = 'comic'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let intent = f.create("correct-type").await;
    f.submit(&intent.id).await;
    f.server.seen.lock().unwrap().completed = true;
    for _ in 0..2 {
        f.pipeline
            .tick_job(f.settings.clone(), &intent.id)
            .await
            .unwrap();
    }
    let changed = FileAssociation {
        source_root_id: f.source_id.clone(),
        source_relative_path: "owned.cbz".into(),
        destination: Some(DestinationSelection {
            root_id: f.destination_id.clone(),
            relative_path: "different.cbz".into(),
        }),
    };
    assert!(matches!(
        f.pipeline
            .associate_file("owner", &intent.id, changed)
            .await,
        Err(PipelineError::Conflict)
    ));
    assert_eq!(
        f.pipeline.get("owner", &intent.id).await.unwrap().state,
        AcquisitionState::Downloaded
    );
}

#[tokio::test]
async fn read_scope_cannot_search_submit_or_associate() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let f = Fixture::new(ContentType::Comic, false).await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    auth.setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let key = auth
        .create_key("read-key", crate::auth::Scope::Read)
        .await
        .unwrap();
    let context = crate::httpapi::auth::AuthContext {
        service: auth,
        origin: "http://localhost".into(),
    };
    let app = crate::httpapi::search::routes(crate::httpapi::search::SearchContext {
        search: Search::new(f.store.clone(), f.settings.clone()),
        auth: context.clone(),
    })
    .merge(crate::httpapi::acquisition::routes(
        crate::httpapi::acquisition::AcquisitionContext {
            pipeline: f.pipeline.clone(),
            settings: f.settings.clone(),
            auth: context,
        },
    ));
    for (method, path, body) in [
        (
            "GET",
            "/api/v1/search/metadata?integration_id=unused&query=fixture",
            serde_json::json!({}),
        ),
        (
            "POST",
            "/api/v1/search/releases",
            serde_json::json!({"integration_id":"unused","content_type":"comic","query":"fixture"}),
        ),
        (
            "POST",
            "/api/v1/search/release-assessments",
            serde_json::json!({"release_handle":f.request.release_handle,"unit_id":f.request.unit_id}),
        ),
        (
            "POST",
            "/api/v1/acquisition",
            serde_json::to_value(&f.request).unwrap(),
        ),
        (
            "POST",
            "/api/v1/acquisition/unused/files",
            serde_json::json!({"source_root_id":f.source_id,"source_relative_path":"owned.cbz","destination":{"root_id":f.destination_id,"relative_path":"selected.cbz"}}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {}", key.secret))
                    .header("content-type", "application/json")
                    .header("idempotency-key", "denied")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn manage_discovery_exposes_choices_without_private_configuration() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let f = Fixture::new(ContentType::Comic, false).await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    let login = auth
        .setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let key = auth
        .create_key("manage-key", crate::auth::Scope::Manage)
        .await
        .unwrap();
    let context = crate::httpapi::auth::AuthContext {
        service: auth,
        origin: "http://localhost".into(),
    };
    let app = crate::httpapi::search::routes(crate::httpapi::search::SearchContext {
        search: Search::new(f.store.clone(), f.settings.clone()),
        auth: context.clone(),
    })
    .merge(crate::httpapi::acquisition::routes(
        crate::httpapi::acquisition::AcquisitionContext {
            pipeline: f.pipeline.clone(),
            settings: f.settings.clone(),
            auth: context,
        },
    ));
    for path in ["/api/v1/search/integrations", "/api/v1/acquisition/roots"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {}", key.secret))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        let text = body.to_string();
        for forbidden in [
            "http:",
            "base_url",
            "api_key",
            "password",
            "category",
            "relative_path",
            "local_path",
            "remote_path",
            "fixture-private",
        ] {
            assert!(!text.contains(forbidden));
        }
    }
    let cached_handle = Uuid::new_v4().to_string();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO search_releases (handle, owner, integration_id, source_fingerprint, indexer_id, guid_digest, content_type, protocol, query, search_offset, search_limit, expires_at, evidence_json) SELECT ?, ?, integration_id, source_fingerprint, indexer_id, guid_digest, content_type, protocol, query, search_offset, search_limit, expires_at, evidence_json FROM search_releases WHERE handle = ?")
        .bind(&cached_handle)
        .bind(&login.principal.user_id)
        .bind(&f.request.release_handle)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let provider_requests = f.server.seen.lock().unwrap().requests.len();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/search/release-assessments")
                .header("authorization", format!("Bearer {}", key.secret))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"release_handle":cached_handle,"unit_id":f.request.unit_id})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body["assessment_id"].is_string());
    assert!(body["assessment_expires_at"].is_number());
    assert!(body["evaluation"].is_object());
    assert!(body["active_rejection"].is_null());
    assert_eq!(body["target"]["unit"]["id"], f.request.unit_id);
    assert_eq!(
        f.server.seen.lock().unwrap().requests.len(),
        provider_requests
    );
    let acquired = f.create("safe-destination").await;
    assert!(acquired.destination_reserved);
    assert!(
        !serde_json::to_string(&acquired)
            .unwrap()
            .contains("selected.cbz")
    );
}
