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
    async fn stop_polling(&self, id: &str) -> AcquisitionReview {
        self.server.seen.lock().unwrap().category = Some("foreign".into());
        self.pipeline
            .tick_job(self.settings.clone(), id)
            .await
            .unwrap();
        self.server.seen.lock().unwrap().category = None;
        self.pipeline.review("owner", id).await.unwrap()
    }
    async fn failed_mapping(&self, id: &str) -> AcquisitionReview {
        self.submit(id).await;
        self.server.seen.lock().unwrap().completed = true;
        let receipts = self.pipeline.get("owner", id).await.unwrap().receipt_count;
        for _ in 0..receipts {
            self.pipeline
                .tick_job(self.settings.clone(), id)
                .await
                .unwrap();
        }
        self.pipeline
            .associate_file(
                "owner",
                id,
                FileAssociation {
                    source_root_id: self.source_id.clone(),
                    source_relative_path: "missing.cbz".into(),
                    destination: None,
                },
            )
            .await
            .unwrap();
        self.pipeline
            .tick_job(self.settings.clone(), id)
            .await
            .unwrap();
        let review = self.pipeline.review("owner", id).await.unwrap();
        assert_eq!(
            review.acquisition.reason,
            Some(AcquisitionReason::ImportReviewRequired)
        );
        assert_eq!(review.lifecycle, ReviewLifecycle::Returned);
        let journals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM import_operations")
            .fetch_one(self.store.reader())
            .await
            .unwrap();
        assert_eq!(journals, 0);
        review
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

fn mapping_request(f: &Fixture, review: &AcquisitionReview) -> CorrectMapping {
    CorrectMapping {
        revision: review.revision,
        polling_fingerprint: review.polling_fingerprint.clone(),
        source_root_id: f.source_id.clone(),
        source_relative_path: "owned.cbz".into(),
    }
}

impl Fixture {
    async fn import_review(
        &self,
        id: &str,
        policy: ImportPolicy,
        phase: ImportPhase,
    ) -> AcquisitionReview {
        let review = self.failed_mapping(id).await;
        let mut tx = self.store.begin_write().await.unwrap();
        sqlx::query("UPDATE acquisition_runs SET source_relative = 'owned.cbz' WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let service = ImportService::new(self.store.clone());
        let import_id = review.acquisition.import_id.unwrap();
        let torrent = !self.server.seen.lock().unwrap().expected_hash.is_empty();
        let mut op = service
            .plan(
                &import_id,
                InternalImportRequest {
                    source_root: self.source_id.clone(),
                    source_relative: "owned.cbz".into(),
                    destination_root: self.destination_id.clone(),
                    destination_relative: "selected.cbz".into(),
                    unit_id: self.request.unit_id.clone(),
                    policy,
                },
                if torrent {
                    ImportMode::TorrentPreserving
                } else {
                    ImportMode::Standalone
                },
            )
            .await
            .unwrap();
        while op.phase != phase {
            op = service.step(&import_id).await.unwrap();
        }
        self.pipeline.review("owner", id).await.unwrap()
    }
}

fn import_request(review: &AcquisitionReview) -> ResumeImport {
    ResumeImport {
        revision: review.revision,
        polling_fingerprint: review.polling_fingerprint.clone(),
    }
}

#[tokio::test]
async fn resume_import_fences_revision_replay_races_and_reopen_without_client_writes() {
    for (torrent, policy, phase) in [
        (false, ImportPolicy::Copy, ImportPhase::Planned),
        (true, ImportPolicy::Copy, ImportPhase::Verified),
        (
            false,
            ImportPolicy::Hardlink {
                fallback_to_copy: false,
            },
            ImportPhase::Staged,
        ),
        (false, ImportPolicy::Move, ImportPhase::CleanupPending),
    ] {
        let f = Fixture::new(ContentType::Comic, torrent).await;
        let intent = f.create("resume-import").await;
        let stale = f.import_review(&intent.id, policy.clone(), phase).await;
        let mut tx = f.store.begin_write().await.unwrap();
        lifecycle_event(&mut tx, &intent.id, "import_test", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            f.pipeline
                .resume_import(&f.settings, "owner", &intent.id, import_request(&stale))
                .await,
            Err(PipelineError::Conflict)
        ));
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            f.pipeline
                .resume_import(&f.settings, "other", &intent.id, import_request(&review))
                .await,
            Err(PipelineError::NotFound)
        ));
        let import_id = review.acquisition.import_id.as_ref().unwrap();
        let before: (String, String) = sqlx::query_as(
            "SELECT request_json, identity_json FROM import_operations WHERE id = ?",
        )
        .bind(import_id)
        .fetch_one(f.store.reader())
        .await
        .unwrap();
        let fence: (bool, String, Option<String>) = sqlx::query_as(
            "SELECT attempted, payload_digest, torrent_hash FROM acquisition_runs WHERE id = ?",
        )
        .bind(&intent.id)
        .fetch_one(f.store.reader())
        .await
        .unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        f.store.close().await;
        let store = SqliteStore::open(f.dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        );
        let pipeline = Pipeline::new(store.clone());
        let reopened = pipeline.review("owner", &intent.id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&review).unwrap(),
            serde_json::to_value(&reopened).unwrap()
        );
        let (a, b) = tokio::join!(
            pipeline.resume_import(&settings, "owner", &intent.id, import_request(&review)),
            pipeline.resume_import(&settings, "owner", &intent.id, import_request(&review))
        );
        assert!(matches!(
            (&a, &b),
            (Ok(_), Err(PipelineError::Conflict)) | (Err(PipelineError::Conflict), Ok(_))
        ));
        assert!(matches!(
            pipeline
                .resume_import(&settings, "owner", &intent.id, import_request(&review))
                .await,
            Err(PipelineError::Conflict)
        ));
        for _ in 0..12 {
            pipeline
                .tick_job(settings.clone(), &intent.id)
                .await
                .unwrap();
        }
        assert_eq!(
            pipeline.get("owner", &intent.id).await.unwrap().state,
            AcquisitionState::Completed
        );
        let service = ImportService::new(store.clone());
        for _ in 0..2 {
            assert_eq!(
                service.recover(import_id).await.unwrap().phase,
                ImportPhase::Done
            );
        }
        let after: (String, String) = sqlx::query_as(
            "SELECT request_json, identity_json FROM import_operations WHERE id = ?",
        )
        .bind(import_id)
        .fetch_one(store.reader())
        .await
        .unwrap();
        assert_eq!(before, after);
        assert_eq!(service.get(import_id).await.unwrap().request.policy, policy);
        let after_fence: (bool, String, Option<String>) = sqlx::query_as(
            "SELECT attempted, payload_digest, torrent_hash FROM acquisition_runs WHERE id = ?",
        )
        .bind(&intent.id)
        .fetch_one(store.reader())
        .await
        .unwrap();
        assert_eq!(fence, after_fence);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM import_operations")
            .fetch_one(store.reader())
            .await
            .unwrap();
        assert_eq!(count, 1);
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_events WHERE job_id = ? AND event = 'acquisition_import_resumed' AND from_state = 'needs_review' AND to_state = 'running'")
            .bind(&intent.job_id).fetch_one(store.reader()).await.unwrap();
        assert_eq!(audits, 1);
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
        assert_eq!(
            f.source.path().join("owned.cbz").exists(),
            policy != ImportPolicy::Move
        );
    }
}

#[tokio::test]
async fn resume_import_rejects_unsafe_journals_receipts_configuration_and_work() {
    for change in [
        "DELETE FROM import_operations WHERE id = (SELECT import_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE import_operations SET phase = 'done' WHERE id = (SELECT import_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE import_operations SET request_json = json_set(request_json, '$.source_relative', 'other.cbz') WHERE id = (SELECT import_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE import_operations SET identity_json = json_set(identity_json, '$.destination_root', '/foreign') WHERE id = (SELECT import_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE import_operations SET destination_relative = 'other.cbz' WHERE id = (SELECT import_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE acquisition_runs SET selection_decision_id = NULL WHERE id = ?",
        "UPDATE acquisition_runs SET source_relative = 'other.cbz' WHERE id = ?",
        "UPDATE acquisition_runs SET reason = 'uncertain_submission' WHERE id = ?",
        "UPDATE acquisition_runs SET work_token = 'expired', work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET work_until = unixepoch() + 300 WHERE id = ?",
        "UPDATE jobs SET worker = 'expired', lease_until = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET retry_at = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET state = 'cancel_requested' WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_lifecycle_v1' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_claim_returned' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM acquisition_receipts WHERE acquisition_id = ?",
        "UPDATE acquisition_receipts SET completed = 0 WHERE acquisition_id = ?",
        "UPDATE acquisition_receipts SET external_id = 'invalid' WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_receipts SET ordinal = -1 WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE search_releases SET owner = 'foreign' WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?)",
        "UPDATE acquisition_intents SET request = json_set(request, '$.client_id', 'foreign') WHERE id = ?",
        "UPDATE integrations SET base_url = 'http://127.0.0.1:1' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET enabled = 0 WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET secret_nonce = zeroblob(24), secret_ciphertext = zeroblob(28) WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("unsafe-import").await;
        let stale = f
            .import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
            .await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query(change)
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        for snapshot in [&stale, &review] {
            assert!(
                matches!(
                    f.pipeline
                        .resume_import(&f.settings, "owner", &intent.id, import_request(snapshot))
                        .await,
                    Err(PipelineError::Conflict)
                ),
                "{change}"
            );
        }
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        assert_eq!(
            review.revision,
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision
        );
        assert!(!f.destination.path().join("selected.cbz").exists());
    }
}

#[tokio::test]
async fn resume_import_rejects_torrent_move_and_mismatched_modes() {
    for (torrent, change) in [
        (
            true,
            "UPDATE import_operations SET request_json = json_set(request_json, '$.policy', 'Move')",
        ),
        (
            false,
            "UPDATE import_operations SET identity_json = json_set(identity_json, '$.mode', 'TorrentPreserving')",
        ),
    ] {
        let f = Fixture::new(ContentType::Comic, torrent).await;
        let intent = f.create("import-mode").await;
        f.import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
            .await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query(change).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        assert!(
            matches!(
                f.pipeline
                    .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
                    .await,
                Err(PipelineError::Conflict)
            ),
            "{change}"
        );
        assert_eq!(
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision,
            review.revision
        );
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
}

#[tokio::test]
async fn resumed_import_rechecks_mode_before_advancing_the_journal() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("tick-import-mode").await;
    let review = f
        .import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
        .await;
    f.pipeline
        .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
        .await
        .unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE import_operations SET identity_json = json_set(identity_json, '$.mode', 'TorrentPreserving')").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let requests = f.server.seen.lock().unwrap().requests.clone();
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let acquisition = f.pipeline.get("owner", &intent.id).await.unwrap();
    assert_eq!(acquisition.state, AcquisitionState::NeedsReview);
    assert_eq!(
        acquisition.reason,
        Some(AcquisitionReason::ImportReviewRequired)
    );
    let service = ImportService::new(f.store.clone());
    assert_eq!(
        service
            .get(review.acquisition.import_id.as_ref().unwrap())
            .await
            .unwrap()
            .phase,
        ImportPhase::Planned
    );
    assert!(!f.destination.path().join("selected.cbz").exists());
    assert_eq!(requests, f.server.seen.lock().unwrap().requests);
}

#[tokio::test]
async fn import_resume_transaction_never_waits_for_hash_slots_and_rechecks_filesystem_facts() {
    for changed in [false, true] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("import-preflight").await;
        let review = f
            .import_review(&intent.id, ImportPolicy::Move, ImportPhase::CleanupPending)
            .await;
        let request = import_request(&review);
        let validated = f
            .pipeline
            .prepare_import("owner", &intent.id, &request)
            .await
            .unwrap();
        let slots = crate::library::roots::hash_slots();
        let _permits = slots.acquire_many(2).await.unwrap();
        if changed {
            let source = f.source.path().join("owned.cbz");
            let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
            let mut bytes = std::fs::read(&source).unwrap();
            bytes[0] ^= 1;
            std::fs::write(&source, bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(source)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.pipeline
                .apply_import(&f.settings, "owner", &intent.id, &request, validated),
        )
        .await
        .expect("the write transaction must not acquire a hash slot");
        if changed {
            assert!(matches!(result, Err(PipelineError::ImportCleanupReview)));
            assert_eq!(
                f.pipeline
                    .review("owner", &intent.id)
                    .await
                    .unwrap()
                    .revision,
                review.revision
            );
        } else {
            assert_eq!(result.unwrap().state, AcquisitionState::Importing);
        }
    }
}

#[tokio::test]
async fn import_cleanup_errors_only_recommend_review_for_changed_evidence() {
    use axum::{http::StatusCode, response::IntoResponse};
    use http_body_util::BodyExt;
    for (error, status, code) in [
        (
            ImportError::Changed,
            StatusCode::CONFLICT,
            "import_cleanup_review_required",
        ),
        (
            ImportError::Worker,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
        ),
        (
            ImportError::Io(std::io::ErrorKind::WouldBlock.into()),
            StatusCode::CONFLICT,
            "acquisition_conflict",
        ),
        (
            ImportError::Io(std::io::ErrorKind::NotFound.into()),
            StatusCode::CONFLICT,
            "acquisition_conflict",
        ),
        (
            ImportError::UnsupportedMove,
            StatusCode::CONFLICT,
            "acquisition_conflict",
        ),
    ] {
        let response =
            crate::httpapi::errors::ApiError::from(import_cleanup_error(error)).into_response();
        assert_eq!(response.status(), status);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"]["code"], code);
        if code != "import_cleanup_review_required" {
            assert!(
                !body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("abandon")
            );
        }
    }
}

#[tokio::test]
async fn resume_import_unavailable_or_replaced_source_root_is_a_generic_conflict() {
    for replaced in [false, true] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("unavailable-root").await;
        let review = f
            .import_review(&intent.id, ImportPolicy::Move, ImportPhase::CleanupPending)
            .await;
        let unavailable = f.source.path().with_extension("unavailable");
        std::fs::rename(f.source.path(), &unavailable).unwrap();
        if replaced {
            std::fs::create_dir(f.source.path()).unwrap();
        }
        let result = f
            .pipeline
            .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
            .await;
        if replaced {
            std::fs::remove_dir(f.source.path()).unwrap();
        }
        std::fs::rename(unavailable, f.source.path()).unwrap();
        assert!(matches!(result, Err(PipelineError::Conflict)));
        assert_eq!(
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision,
            review.revision
        );
    }
}

#[tokio::test]
async fn resume_import_requires_actual_returns_and_importer_lock_and_never_replans() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("locked-import").await;
    let review = f
        .import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
        .await;
    let service = ImportService::new(f.store.clone());
    let import_id = review.acquisition.import_id.as_ref().unwrap();
    let lock = finalize::lock(&service.get(import_id).await.unwrap()).unwrap();
    assert!(matches!(
        f.pipeline
            .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
            .await,
        Err(PipelineError::Conflict)
    ));
    drop(lock);
    let mut tx = f.store.begin_write().await.unwrap();
    lifecycle_event(
        &mut tx,
        &intent.id,
        "acquisition_claim_started",
        Some("lost"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    f.store.close().await;
    let store = SqliteStore::open(f.dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
    );
    let pipeline = Pipeline::new(store.clone());
    let unproven = pipeline.review("owner", &intent.id).await.unwrap();
    assert!(matches!(
        pipeline
            .resume_import(&settings, "owner", &intent.id, import_request(&unproven))
            .await,
        Err(PipelineError::Conflict)
    ));
    let mut tx = store.begin_write().await.unwrap();
    lifecycle_event(
        &mut tx,
        &intent.id,
        "acquisition_claim_returned",
        Some("lost"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let returned = pipeline.review("owner", &intent.id).await.unwrap();
    pipeline
        .resume_import(&settings, "owner", &intent.id, import_request(&returned))
        .await
        .unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("DELETE FROM import_operations WHERE id = ?")
        .bind(import_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    pipeline.tick_job(settings, &intent.id).await.unwrap();
    assert_eq!(
        pipeline.get("owner", &intent.id).await.unwrap().state,
        AcquisitionState::NeedsReview
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM import_operations")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(!f.destination.path().join("selected.cbz").exists());
}

#[tokio::test]
async fn resume_import_move_changed_cleanup_requires_review_or_abandonment() {
    for (recorded, captured) in [(false, false), (true, false), (false, true)] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("move-review").await;
        let stale = f
            .import_review(&intent.id, ImportPolicy::Move, ImportPhase::CleanupPending)
            .await;
        let import_id = stale.acquisition.import_id.as_ref().unwrap();
        let service = ImportService::new(f.store.clone());
        let source = if captured {
            service.step(import_id).await.unwrap();
            f.source
                .path()
                .join(format!(".library-move-{import_id}/source"))
        } else {
            f.source.path().join("owned.cbz")
        };
        let original = std::fs::read(&source).unwrap();
        if recorded {
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query("UPDATE import_operations SET last_error = 'file identity or content changed; review is required' WHERE id = ?").bind(import_id).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        } else {
            std::fs::write(&source, b"changed source").unwrap();
        }
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        assert!(matches!(
            f.pipeline
                .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
                .await,
            Err(PipelineError::ImportCleanupReview)
        ));
        assert_eq!(
            review.revision,
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision
        );
        assert_eq!(
            std::fs::read(&source).unwrap(),
            if recorded {
                original
            } else {
                b"changed source".to_vec()
            }
        );
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        assert_eq!(
            service.abandon_move_cleanup(import_id).await.unwrap().phase,
            ImportPhase::Done
        );
        let done = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            f.pipeline
                .resume_import(&f.settings, "owner", &intent.id, import_request(&done))
                .await,
            Err(PipelineError::Conflict)
        ));
    }
}

#[tokio::test]
async fn mapping_correction_fences_stale_replayed_and_racing_actions_across_reopen() {
    for torrent in [false, true] {
        let f = Fixture::new(ContentType::Comic, torrent).await;
        let intent = f.create("mapping").await;
        let stale = f.failed_mapping(&intent.id).await;
        let mut tx = f.store.begin_write().await.unwrap();
        lifecycle_event(&mut tx, &intent.id, "mapping_test", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            f.pipeline
                .correct_mapping(
                    &f.settings,
                    "owner",
                    &intent.id,
                    mapping_request(&f, &stale)
                )
                .await,
            Err(PipelineError::Conflict)
        ));
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert!(matches!(
            f.pipeline
                .correct_mapping(
                    &f.settings,
                    "other",
                    &intent.id,
                    mapping_request(&f, &review)
                )
                .await,
            Err(PipelineError::NotFound)
        ));
        let receipts: Vec<(i64, String, bool)> = sqlx::query_as("SELECT ordinal, external_id, completed FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal")
            .bind(&intent.id).fetch_all(f.store.reader()).await.unwrap();
        let fence: (bool, String, Option<String>, String, String, String) = sqlx::query_as("SELECT attempted, payload_digest, torrent_hash, destination_root, destination_relative, destination_path FROM acquisition_runs WHERE id = ?")
            .bind(&intent.id).fetch_one(f.store.reader()).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        f.store.close().await;
        let store = SqliteStore::open(f.dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        );
        let pipeline = Pipeline::new(store.clone());
        let other = Pipeline::new(SqliteStore::open(f.dir.path()).await.unwrap());
        let reopened = pipeline.review("owner", &intent.id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&review).unwrap(),
            serde_json::to_value(&reopened).unwrap()
        );
        let (a, b) = tokio::join!(
            pipeline.correct_mapping(&settings, "owner", &intent.id, mapping_request(&f, &review)),
            other.correct_mapping(&settings, "owner", &intent.id, mapping_request(&f, &review))
        );
        assert!(matches!(
            (&a, &b),
            (Ok(_), Err(PipelineError::Conflict)) | (Err(PipelineError::Conflict), Ok(_))
        ));
        assert!(matches!(
            pipeline
                .correct_mapping(&settings, "owner", &intent.id, mapping_request(&f, &review))
                .await,
            Err(PipelineError::Conflict)
        ));
        let after = pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(after.state, AcquisitionState::Importing);
        assert_eq!(after.import_id, review.acquisition.import_id);
        let after_fence: (bool, String, Option<String>, String, String, String) = sqlx::query_as("SELECT attempted, payload_digest, torrent_hash, destination_root, destination_relative, destination_path FROM acquisition_runs WHERE id = ?")
            .bind(&intent.id).fetch_one(store.reader()).await.unwrap();
        assert_eq!(fence, after_fence);
        let after_receipts: Vec<(i64, String, bool)> = sqlx::query_as("SELECT ordinal, external_id, completed FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal")
            .bind(&intent.id).fetch_all(store.reader()).await.unwrap();
        assert_eq!(receipts, after_receipts);
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_events WHERE job_id = ? AND event = 'acquisition_mapping_corrected' AND from_state = 'needs_review' AND to_state = 'running'")
            .bind(&intent.job_id).fetch_one(store.reader()).await.unwrap();
        assert_eq!(audits, 1);
        for _ in 0..10 {
            pipeline
                .tick_job(settings.clone(), &intent.id)
                .await
                .unwrap();
        }
        let completed = pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(completed.state, AcquisitionState::Completed);
        assert_eq!(completed.import_id, after.import_id);
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
        assert_eq!(
            std::fs::read(f.source.path().join("owned.cbz")).unwrap(),
            std::fs::read(f.destination.path().join("selected.cbz")).unwrap()
        );
    }
}

#[tokio::test]
async fn mapping_correction_binds_blocking_validation_to_revision_and_paths() {
    for change in ["revision", "relative_path", "source_root", "file_removed"] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("mapping-validation").await;
        let review = f.failed_mapping(&intent.id).await;
        let mut request = mapping_request(&f, &review);
        let alternate = tempfile::tempdir().unwrap();
        if change == "source_root" {
            std::fs::copy(
                f.source.path().join("owned.cbz"),
                alternate.path().join("owned.cbz"),
            )
            .unwrap();
            request.source_root_id = Library::new(f.store.clone())
                .register_root("alternate", alternate.path())
                .await
                .unwrap()
                .id;
        }
        let validated = f
            .pipeline
            .prepare_mapping("owner", &intent.id, &request)
            .await
            .unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        let mut tx = f.store.begin_write().await.unwrap();
        match change {
            "revision" => lifecycle_event(&mut tx, &intent.id, "mapping_test", None)
                .await
                .unwrap(),
            "relative_path" => request.source_relative_path = "different.cbz".into(),
            "source_root" => {
                sqlx::query("UPDATE library_roots SET path = ? WHERE id = ?")
                    .bind(alternate.path().join("changed").to_str().unwrap())
                    .bind(&request.source_root_id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            "file_removed" => {}
            _ => unreachable!(),
        }
        tx.commit().await.unwrap();
        if change == "revision" {
            request.revision = f
                .pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision;
            assert!(request.revision > validated.revision);
        }
        // Applying a prepared mapping uses DB facts only, even when the source is now absent.
        std::fs::remove_file(f.source.path().join("owned.cbz")).unwrap();
        let result = f
            .pipeline
            .apply_mapping(&f.settings, "owner", &intent.id, &request, validated)
            .await;
        if change == "file_removed" {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(PipelineError::Conflict)), "{change}");
        }
        let after = f.pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(
            after.state,
            if change == "file_removed" {
                AcquisitionState::Importing
            } else {
                AcquisitionState::NeedsReview
            }
        );
        assert_eq!(after.import_id, review.acquisition.import_id);
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
}

#[tokio::test]
async fn mapping_correction_rejects_unsafe_work_and_journals_without_client_io_or_audit() {
    for change in [
        "DELETE FROM acquisition_receipts WHERE acquisition_id = ?",
        "UPDATE acquisition_receipts SET completed = 0 WHERE acquisition_id = ?",
        "UPDATE acquisition_receipts SET external_id = 'invalid' WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_receipts SET ordinal = -1 WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_runs SET reason = 'uncertain_submission' WHERE id = ?",
        "UPDATE acquisition_runs SET attempted = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET import_id = NULL WHERE id = ?",
        "UPDATE acquisition_runs SET source_root = NULL, source_relative = NULL WHERE id = ?",
        "UPDATE acquisition_runs SET work_token = 'expired', work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'queued' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'downloading' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'importing' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'completed' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'canceled' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'downloaded' WHERE id = ?",
        "UPDATE acquisition_runs SET destination_path = '/invalid.cbz' WHERE id = ?",
        "UPDATE acquisition_runs SET category = 'foreign' WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request, '$.client_id', 'foreign') WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request, '$.unit_id', 'foreign') WHERE id = ?",
        "UPDATE search_releases SET owner = 'foreign' WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?)",
        "UPDATE jobs SET worker = 'expired', lease_until = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET retry_at = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET state = 'cancel_requested' WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'client_accepted' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_lifecycle_v1' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_claim_returned' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase) SELECT import_id,source_root,destination_root,destination_relative,destination_path,unit_id,'{}','{}','planned' FROM acquisition_runs WHERE id = ?",
        "INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase) SELECT 'other-journal',source_root,destination_root,destination_relative,destination_path,unit_id,'{}','{}','done' FROM acquisition_runs WHERE id = ?",
        "INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase) SELECT import_id,source_root,destination_root,destination_relative || '.other',destination_path || '.other','unrelated-unit','{}','{}','planned' FROM acquisition_runs WHERE id = ?",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("unsafe-mapping").await;
        let review = f.failed_mapping(&intent.id).await;
        let mut tx = f.store.begin_write().await.unwrap();
        if change.contains("unrelated-unit") {
            sqlx::query("INSERT INTO units(id,edition_id,label,kind) SELECT 'unrelated-unit',edition_id,'unrelated','issue' FROM units WHERE id = ?")
                .bind(&intent.unit_id).execute(&mut *tx).await.unwrap();
        }
        sqlx::query(change)
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        f.store.close().await;
        let store = SqliteStore::open(f.dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        );
        let pipeline = Pipeline::new(store.clone());
        let snapshot = pipeline.review("owner", &intent.id).await.unwrap_or(review);
        let requests = f.server.seen.lock().unwrap().requests.clone();
        assert!(
            matches!(
                pipeline
                    .correct_mapping(
                        &settings,
                        "owner",
                        &intent.id,
                        mapping_request(&f, &snapshot)
                    )
                    .await,
                Err(PipelineError::Conflict)
            ),
            "{change}"
        );
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_events WHERE job_id = ? AND event = 'acquisition_mapping_corrected'")
            .bind(&intent.job_id).fetch_one(store.reader()).await.unwrap();
        assert_eq!(audits, 0, "{change}");
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
}

#[tokio::test]
async fn mapping_correction_rechecks_changed_evidence_and_rolls_back_guarded_updates() {
    for change in [
        "UPDATE acquisition_receipts SET external_id = 'SABnzbd_nzo_changed' WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE integrations SET enabled = 0 WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET base_url = 'http://127.0.0.1:1' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET options = '{\"category\":\"changed\"}' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET options = json_set(options, '$.remote_path', '/remote', '$.local_path', '/local') WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET secret_nonce = zeroblob(24), secret_ciphertext = zeroblob(28) WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE library_roots SET path = '/changed' WHERE id = (SELECT destination_root FROM acquisition_runs WHERE id = ?)",
        "CREATE TRIGGER ignore_mapping BEFORE UPDATE ON acquisition_runs BEGIN SELECT RAISE(IGNORE); END",
        "CREATE TRIGGER ignore_mapping BEFORE UPDATE ON jobs BEGIN SELECT RAISE(IGNORE); END",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("changed-mapping").await;
        let review = f.failed_mapping(&intent.id).await;
        let requests = f.server.seen.lock().unwrap().requests.clone();
        let mut tx = f.store.begin_write().await.unwrap();
        let query = sqlx::query(change);
        if change.starts_with("CREATE") {
            query.execute(&mut *tx).await.unwrap();
        } else {
            query.bind(&intent.id).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        let changed = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert_eq!(changed.revision, review.revision);
        assert!(
            matches!(
                f.pipeline
                    .correct_mapping(
                        &f.settings,
                        "owner",
                        &intent.id,
                        mapping_request(&f, &review)
                    )
                    .await,
                Err(PipelineError::Conflict)
            ),
            "{change}"
        );
        if !change.contains("external_id") {
            assert!(
                matches!(
                    f.pipeline
                        .correct_mapping(
                            &f.settings,
                            "owner",
                            &intent.id,
                            mapping_request(&f, &changed)
                        )
                        .await,
                    Err(PipelineError::Conflict)
                ),
                "{change}"
            );
        }
        let after = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&changed).unwrap(),
            serde_json::to_value(&after).unwrap(),
            "{change}"
        );
        let job: (String, Option<String>) =
            sqlx::query_as("SELECT state, reason FROM jobs WHERE id = ?")
                .bind(&intent.job_id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
        assert_eq!(
            job,
            ("needs_review".into(), Some("import_review_required".into()))
        );
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
}

#[tokio::test]
async fn mapping_correction_rejects_paths_outside_registered_roots() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("mapping-paths").await;
    let review = f.failed_mapping(&intent.id).await;
    symlink(f.destination.path(), f.source.path().join("escape")).unwrap();
    symlink(
        f.source.path().join("owned.cbz"),
        f.source.path().join("linked.cbz"),
    )
    .unwrap();
    let requests = f.server.seen.lock().unwrap().requests.clone();
    for (path, expected) in [
        ("/owned.cbz", PipelineError::Invalid),
        ("../owned.cbz", PipelineError::Invalid),
        ("sub/../../owned.cbz", PipelineError::Invalid),
        ("./owned.cbz", PipelineError::Invalid),
        ("a//owned.cbz", PipelineError::Invalid),
        ("a\\owned.cbz", PipelineError::Invalid),
        ("escape/selected.cbz", PipelineError::Invalid),
        ("linked.cbz", PipelineError::Invalid),
        (".library-imports/owned.cbz", PipelineError::Invalid),
        ("missing.cbz", PipelineError::Conflict),
        ("different-missing.cbz", PipelineError::Filesystem),
        ("owned.txt", PipelineError::Invalid),
        ("owned.pdf", PipelineError::Invalid),
        ("owned.cbr", PipelineError::Invalid),
    ] {
        let mut request = mapping_request(&f, &review);
        request.source_relative_path = path.into();
        let error = f
            .pipeline
            .correct_mapping(&f.settings, "owner", &intent.id, request)
            .await
            .unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected),
            "{path}: {error:?}"
        );
        assert_eq!(
            review.revision,
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision
        );
    }
    let mut request = mapping_request(&f, &review);
    request.source_root_id = Uuid::new_v4().to_string();
    assert!(matches!(
        f.pipeline
            .correct_mapping(&f.settings, "owner", &intent.id, request)
            .await,
        Err(PipelineError::NotFound)
    ));
    std::fs::create_dir(f.destination.path().join("nested")).unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET destination_relative = 'nested/selected.cbz', destination_path = ? WHERE id = ?")
        .bind(f.destination.path().join("nested/selected.cbz").to_str().unwrap()).bind(&intent.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let changed = f.pipeline.review("owner", &intent.id).await.unwrap();
    std::fs::remove_dir(f.destination.path().join("nested")).unwrap();
    symlink(f.source.path(), f.destination.path().join("nested")).unwrap();
    assert!(matches!(
        f.pipeline
            .correct_mapping(
                &f.settings,
                "owner",
                &intent.id,
                mapping_request(&f, &changed)
            )
            .await,
        Err(PipelineError::Invalid)
    ));
    assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    let after = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert_eq!(after.revision, review.revision);
    assert!(!f.destination.path().join("selected.cbz").exists());
}

fn resume_request(review: &AcquisitionReview) -> ResumePolling {
    ResumePolling {
        revision: review.revision,
        polling_fingerprint: review.polling_fingerprint.clone(),
    }
}

#[tokio::test]
async fn resume_polling_fences_stale_replayed_and_racing_actions_across_reopen() {
    for torrent in [false, true] {
        let f = Fixture::new(ContentType::Comic, torrent).await;
        let intent = f.create("resume").await;
        f.submit(&intent.id).await;
        let stale = f.stop_polling(&intent.id).await;
        assert_eq!(
            stale.acquisition.reason,
            Some(AcquisitionReason::ClientJobMissing)
        );
        assert_eq!(stale.lifecycle, ReviewLifecycle::Returned);
        f.pipeline
            .resume_polling(&f.settings, "owner", &intent.id, resume_request(&stale))
            .await
            .unwrap();
        let review = f.stop_polling(&intent.id).await;
        assert!(review.revision > stale.revision);
        assert!(matches!(
            f.pipeline
                .resume_polling(&f.settings, "other", &intent.id, resume_request(&review))
                .await,
            Err(PipelineError::NotFound)
        ));
        assert!(matches!(
            f.pipeline
                .resume_polling(&f.settings, "owner", &intent.id, resume_request(&stale))
                .await,
            Err(PipelineError::Conflict)
        ));
        let receipts: Vec<(i64, String, bool)> = sqlx::query_as("SELECT ordinal, external_id, completed FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal")
            .bind(&intent.id).fetch_all(f.store.reader()).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        f.store.close().await;
        let store = SqliteStore::open(f.dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        );
        let pipeline = Pipeline::new(store.clone());
        let other = Pipeline::new(SqliteStore::open(f.dir.path()).await.unwrap());
        let (a, b) = tokio::join!(
            pipeline.resume_polling(&settings, "owner", &intent.id, resume_request(&review)),
            other.resume_polling(&settings, "owner", &intent.id, resume_request(&review))
        );
        assert!(matches!(
            (&a, &b),
            (Ok(_), Err(PipelineError::Conflict)) | (Err(PipelineError::Conflict), Ok(_))
        ));
        assert!(matches!(
            pipeline
                .resume_polling(&settings, "owner", &intent.id, resume_request(&review))
                .await,
            Err(PipelineError::Conflict)
        ));
        let acquisition = pipeline.get("owner", &intent.id).await.unwrap();
        assert_eq!(acquisition.state, AcquisitionState::Downloading);
        assert!(acquisition.submitted);
        let after: Vec<(i64, String, bool)> = sqlx::query_as("SELECT ordinal, external_id, completed FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal")
            .bind(&intent.id).fetch_all(store.reader()).await.unwrap();
        assert_eq!(receipts, after);
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_events WHERE job_id = ? AND event = 'acquisition_polling_resumed' AND from_state = 'needs_review' AND to_state = 'running'")
            .bind(&intent.job_id).fetch_one(store.reader()).await.unwrap();
        assert_eq!(audits, 2);
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        pipeline.tick_job(settings, &intent.id).await.unwrap();
        let seen = f.server.seen.lock().unwrap();
        assert_eq!(seen.submissions, 1);
        assert!(seen.requests.len() > requests.len());
        assert!(seen.requests[requests.len()..].iter().all(|request| {
            request.starts_with("POST /api ")
                || request.contains("/api/v2/auth/login ")
                || request.starts_with("GET /api/v2/app/version ")
                || request.contains("/api/v2/torrents/info?")
        }));
    }
}

#[tokio::test]
async fn resume_polling_rejects_unsafe_evidence_without_client_io_or_audit() {
    for change in [
        "DELETE FROM acquisition_receipts WHERE acquisition_id = ?",
        "UPDATE acquisition_receipts SET external_id = 'invalid' WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_receipts SET ordinal = -1 WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_runs SET reason = 'uncertain_submission' WHERE id = ?",
        "UPDATE acquisition_runs SET reason = 'download_failed' WHERE id = ?",
        "UPDATE acquisition_runs SET import_id = 'existing' WHERE id = ?",
        "UPDATE acquisition_runs SET source_root = destination_root, source_relative = 'owned.cbz' WHERE id = ?",
        "UPDATE acquisition_runs SET work_token = 'expired', work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'downloaded' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'completed' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'canceled' WHERE id = ?",
        "UPDATE acquisition_runs SET category = 'foreign' WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request, '$.client_id', 'foreign') WHERE id = ?",
        "UPDATE search_releases SET owner = 'foreign' WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?)",
        "UPDATE jobs SET worker = 'expired', lease_until = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET state = 'cancel_requested' WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_lifecycle_v1' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_claim_returned' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase) SELECT 'journal',destination_root,destination_root,destination_relative,destination_path,unit_id,'{}','{}','planned' FROM acquisition_runs WHERE id = ?",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("unsafe-resume").await;
        f.submit(&intent.id).await;
        f.stop_polling(&intent.id).await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query(change)
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        if change.contains("download_failed") {
            sqlx::query("UPDATE jobs SET reason = 'download_failed' WHERE id = ?")
                .bind(&intent.job_id)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        assert!(
            matches!(
                f.pipeline
                    .resume_polling(&f.settings, "owner", &intent.id, resume_request(&review))
                    .await,
                Err(PipelineError::Conflict)
            ),
            "{change}"
        );
        assert_eq!(
            review.revision,
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .revision
        );
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
}

#[tokio::test]
async fn resume_polling_rechecks_receipts_and_configuration_without_new_events() {
    for change in [
        "UPDATE acquisition_receipts SET external_id = 'SABnzbd_nzo_changed' WHERE acquisition_id = ? AND ordinal = 0",
        "UPDATE acquisition_receipts SET completed = 1 WHERE acquisition_id = ?",
        "UPDATE integrations SET label = 'changed' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET enabled = 0 WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET base_url = 'http://127.0.0.1:1' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET options = '{\"category\":\"changed\"}' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET secret_nonce = zeroblob(24), secret_ciphertext = zeroblob(28) WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("changed-resume").await;
        f.submit(&intent.id).await;
        let review = f.stop_polling(&intent.id).await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query(change)
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let changed = f.pipeline.review("owner", &intent.id).await.unwrap();
        assert_eq!(review.revision, changed.revision);
        assert_ne!(review.polling_fingerprint, changed.polling_fingerprint);
        let requests = f.server.seen.lock().unwrap().requests.clone();
        assert!(
            matches!(
                f.pipeline
                    .resume_polling(&f.settings, "owner", &intent.id, resume_request(&review))
                    .await,
                Err(PipelineError::Conflict)
            ),
            "{change}"
        );
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
        if change.contains("enabled")
            || change.contains("base_url")
            || change.contains("options")
            || change.contains("secret_nonce")
        {
            assert!(
                matches!(
                    f.pipeline
                        .resume_polling(&f.settings, "owner", &intent.id, resume_request(&changed))
                        .await,
                    Err(PipelineError::Conflict)
                ),
                "{change}"
            );
        }
    }
}

#[tokio::test]
async fn resume_polling_never_treats_reopen_or_cleared_expired_claim_as_return() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("lost-polling").await;
    f.submit(&intent.id).await;
    let mut tx = f.store.begin_write().await.unwrap();
    lifecycle_event(
        &mut tx,
        &intent.id,
        "acquisition_claim_started",
        Some("lost-worker"),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE acquisition_runs SET work_token = NULL, work_until = NULL WHERE id = ?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let review = f.stop_polling(&intent.id).await;
    assert_eq!(review.lifecycle, ReviewLifecycle::Unproven);
    f.store.close().await;
    let store = SqliteStore::open(f.dir.path()).await.unwrap();
    let settings = Settings::new(
        store.clone(),
        EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
    );
    let pipeline = Pipeline::new(store);
    let reopened = pipeline.review("owner", &intent.id).await.unwrap();
    assert_eq!(review.revision, reopened.revision);
    assert_eq!(reopened.lifecycle, ReviewLifecycle::Unproven);
    let requests = f.server.seen.lock().unwrap().requests.clone();
    assert!(matches!(
        pipeline
            .resume_polling(&settings, "owner", &intent.id, resume_request(&reopened))
            .await,
        Err(PipelineError::Conflict)
    ));
    assert_eq!(requests, f.server.seen.lock().unwrap().requests);
}

#[tokio::test]
async fn reviewed_resumes_roll_back_when_either_guarded_update_changes_no_rows() {
    for recovery in [false, true] {
        for trigger in [
            "CREATE TRIGGER ignore_resume BEFORE UPDATE ON acquisition_runs BEGIN SELECT RAISE(IGNORE); END",
            "CREATE TRIGGER ignore_resume BEFORE UPDATE ON jobs BEGIN SELECT RAISE(IGNORE); END",
        ] {
            let f = Fixture::new(ContentType::Comic, false).await;
            let intent = f.create("guarded-resume").await;
            let review = if recovery {
                f.import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
                    .await
            } else {
                f.submit(&intent.id).await;
                f.stop_polling(&intent.id).await
            };
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query(trigger).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            let result = if recovery {
                f.pipeline
                    .resume_import(&f.settings, "owner", &intent.id, import_request(&review))
                    .await
            } else {
                f.pipeline
                    .resume_polling(&f.settings, "owner", &intent.id, resume_request(&review))
                    .await
            };
            assert!(matches!(result, Err(PipelineError::Conflict)));
            let after = f.pipeline.review("owner", &intent.id).await.unwrap();
            assert_eq!(
                serde_json::to_value(&review).unwrap(),
                serde_json::to_value(&after).unwrap()
            );
            let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
                .bind(&intent.job_id)
                .fetch_one(f.store.reader())
                .await
                .unwrap();
            assert_eq!(state, "needs_review");
            assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
        }
    }
}

#[tokio::test]
async fn reviewed_actions_http_match_contract_and_require_cookie_origin() {
    for action in ["resume-polling", "correct-mapping", "resume-import"] {
        assert_reviewed_action_http(action).await;
    }
}

async fn assert_reviewed_action_http(action: &str) {
    let mapping = action == "correct-mapping";
    let recovery = action == "resume-import";
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("http-resume").await;
    if recovery {
        f.import_review(&intent.id, ImportPolicy::Copy, ImportPhase::Planned)
            .await;
    } else if mapping {
        f.failed_mapping(&intent.id).await;
    } else {
        f.submit(&intent.id).await;
        f.stop_polling(&intent.id).await;
    }
    let auth = crate::auth::AuthService::new(f.store.clone());
    let login = auth
        .setup("owner", "correct horse battery staple")
        .await
        .unwrap();
    if mapping || recovery {
        let app = crate::app::router_with_settings(
            f.store.clone(),
            "http://127.0.0.1:8787",
            f.settings.clone(),
        );
        let review = f.pipeline.review("owner", &intent.id).await.unwrap();
        let mut body = serde_json::json!({"revision":review.revision,"polling_fingerprint":review.polling_fingerprint});
        if mapping {
            body["source_root_id"] = f.source_id.clone().into();
            body["source_relative_path"] = "owned.cbz".into();
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/acquisition/{}/{action}", intent.id))
                    .header(header::COOKIE, format!("library_session={}", login.token))
                    .header(header::ORIGIN, "http://127.0.0.1:8787")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let error: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(error["error"]["code"], "not_found");
    }
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_intents SET caller = ? WHERE id = ?")
        .bind(&login.principal.user_id)
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE search_releases SET owner = ? WHERE handle = ?")
        .bind(&login.principal.user_id)
        .bind(&f.request.release_handle)
        .execute(&mut *tx)
        .await
        .unwrap();
    if recovery {
        sqlx::query("UPDATE release_decisions SET owner = ? WHERE id = ?")
            .bind(&login.principal.user_id)
            .bind(f.request.selection_decision_id.as_ref().unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE release_assessments SET owner = ? WHERE id = (SELECT assessment_id FROM release_decisions WHERE id = ?)")
            .bind(&login.principal.user_id).bind(f.request.selection_decision_id.as_ref().unwrap()).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let app = crate::app::router_with_settings(
        f.store.clone(),
        "http://127.0.0.1:8787",
        f.settings.clone(),
    );
    let review = f
        .pipeline
        .review(&login.principal.user_id, &intent.id)
        .await
        .unwrap();
    let mut body = serde_json::json!({"revision": review.revision, "polling_fingerprint": review.polling_fingerprint});
    if mapping {
        body["source_root_id"] = f.source_id.clone().into();
        body["source_relative_path"] = "owned.cbz".into();
    }
    if mapping {
        for (path, expected_status, expected_code) in [
            ("owned.txt", StatusCode::BAD_REQUEST, "invalid_request"),
            (
                "different-missing.cbz",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ),
            ("owned.cbz", StatusCode::CONFLICT, "acquisition_conflict"),
        ] {
            if expected_status == StatusCode::CONFLICT {
                std::fs::write(f.destination.path().join("selected.cbz"), b"appeared").unwrap();
            }
            let mut failed_body = body.clone();
            failed_body["source_relative_path"] = path.into();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/v1/acquisition/{}/correct-mapping", intent.id))
                        .header(header::COOKIE, format!("library_session={}", login.token))
                        .header(header::ORIGIN, "http://127.0.0.1:8787")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(failed_body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            let error: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(error["error"]["code"], expected_code);
            assert_eq!(
                review.revision,
                f.pipeline
                    .review(&login.principal.user_id, &intent.id)
                    .await
                    .unwrap()
                    .revision
            );
            if expected_status == StatusCode::CONFLICT {
                std::fs::remove_file(f.destination.path().join("selected.cbz")).unwrap();
            }
        }
    }
    let body = body.to_string();
    let requests = f.server.seen.lock().unwrap().requests.clone();
    for origin in [
        None,
        Some("http://127.0.0.1:9999"),
        Some("http://127.0.0.1:8787"),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/acquisition/{}/{action}", intent.id))
            .header(header::COOKIE, format!("library_session={}", login.token))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.clone())).unwrap())
            .await
            .unwrap();
        if origin != Some("http://127.0.0.1:8787") {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let error: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(error["error"]["code"], "invalid_origin");
            assert_eq!(
                f.pipeline
                    .review(&login.principal.user_id, &intent.id)
                    .await
                    .unwrap()
                    .revision,
                review.revision
            );
            continue;
        }
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../api/openapi.yaml")).unwrap();
        let success = &contract["paths"][format!("/api/v1/acquisition/{{id}}/{action}")]["post"]["responses"]
            ["202"];
        assert_eq!(
            success["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Acquisition"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            success["headers"]["Cache-Control"]["schema"]["const"]
                .as_str()
                .unwrap()
        );
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let mut keys: Vec<_> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut required: Vec<_> = contract["components"]["schemas"]["Acquisition"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        keys.sort();
        required.sort();
        assert_eq!(keys, required);
        assert!(body["updated_at"].is_i64());
        assert_eq!(
            body,
            serde_json::json!({"id": intent.id, "job_id": intent.job_id, "unit_id": intent.unit_id, "state": if mapping || recovery { "importing" } else { "downloading" }, "reason": null, "submitted": true, "receipt_count": 2, "import_id": review.acquisition.import_id, "destination_reserved": true, "updated_at": body["updated_at"]})
        );
    }
    assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn terminal_review_checks_ownership_revision_and_lifecycle_without_writes() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("terminal-review").await;
    assert!(matches!(
        f.pipeline.review("owner", &intent.id).await,
        Err(PipelineError::Conflict)
    ));
    assert!(matches!(
        f.pipeline.review("other", &intent.id).await,
        Err(PipelineError::NotFound)
    ));
    f.submit(&intent.id).await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET state = 'needs_review' WHERE id = ?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let first = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert_eq!(first.lifecycle, ReviewLifecycle::Returned);
    assert!(first.revision > 0);
    let replay = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert_eq!(first.revision, replay.revision);
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO job_events(job_id,event,to_state,created_at) VALUES (?,'review_test','needs_review',0)")
        .bind(&intent.job_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let updated = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert!(updated.revision > first.revision);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

#[tokio::test]
async fn terminal_review_fails_closed_on_incomplete_or_malformed_coverage() {
    for change in [
        "DELETE FROM job_events WHERE job_id = ? AND event = 'acquisition_lifecycle_v1'",
        "DELETE FROM job_events WHERE job_id = ? AND event = 'acquisition_claim_returned'",
        "DELETE FROM job_events WHERE job_id = ? AND event = 'acquisition_claim_started'",
        "UPDATE job_events SET worker = NULL WHERE job_id = ? AND event = 'acquisition_claim_started'",
        "UPDATE job_events SET worker = 'malformed' WHERE job_id = ? AND event LIKE 'acquisition_claim_%'",
        "INSERT INTO job_events(job_id,event,to_state,worker,created_at) SELECT job_id,event,to_state,worker,created_at FROM job_events WHERE job_id = ? AND event = 'acquisition_claim_returned'",
        "INSERT INTO job_events(job_id,event,to_state,worker,created_at) SELECT job_id,event,to_state,worker,created_at FROM job_events WHERE job_id = ? AND event = 'acquisition_claim_started'",
        "INSERT INTO job_events(job_id,event,to_state,created_at) VALUES (?,'acquisition_lifecycle_v1','needs_review',0)",
        "UPDATE acquisition_runs SET work_token = 'expired', work_until = 0 WHERE id = (SELECT id FROM acquisition_intents WHERE job_id = ?)",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("malformed-review").await;
        f.submit(&intent.id).await;
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE acquisition_runs SET state = 'needs_review' WHERE id = ?")
            .bind(&intent.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(change)
            .bind(&intent.job_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            f.pipeline
                .review("owner", &intent.id)
                .await
                .unwrap()
                .lifecycle,
            ReviewLifecycle::Unproven,
            "{change}"
        );
        assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
    }
}

#[tokio::test]
async fn lifecycle_evidence_covers_creation_and_each_actual_return() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("lifecycle").await;
    let replay = f.create("lifecycle").await;
    assert_eq!(intent.id, replay.id);
    assert_eq!(
        lifecycle_events(&f.store, &intent.job_id).await,
        vec![("acquisition_lifecycle_v1".into(), None)]
    );
    f.submit(&intent.id).await;
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let events = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(events.len(), 5);
    for pair in events[1..].as_chunks::<2>().0 {
        assert_eq!(pair[0].0, "acquisition_claim_started");
        assert_eq!(pair[1].0, "acquisition_claim_returned");
        assert!(pair[0].1.is_some());
        assert_eq!(pair[0].1, pair[1].1);
    }
    assert_ne!(events[1].1, events[3].1);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 1);
}

async fn lifecycle_events(store: &SqliteStore, job_id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT event, worker FROM job_events WHERE job_id = ? AND event IN ('acquisition_lifecycle_v1', 'acquisition_claim_started', 'acquisition_claim_returned') ORDER BY id")
        .bind(job_id).fetch_all(store.reader()).await.unwrap()
}

#[tokio::test]
async fn lifecycle_displaced_claim_returns_only_when_its_work_returns() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("displaced-lifecycle").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
        seen.own_tag = format!("libraryd-{}", intent.id);
    }
    mock::allow_search(&f.store).await;
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &id).await });
    started.notified().await;
    let events = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(events.len(), 2);
    let old_worker = events[1].1.clone();
    // A competing claim before expiry does not acquire or emit lifecycle evidence.
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    assert_eq!(lifecycle_events(&f.store, &intent.job_id).await, events);
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET work_until = 0 WHERE id = ?")
        .bind(&intent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    // The replacement returns through cancellation without waiting on the blocked server.
    sqlx::query("UPDATE jobs SET state = 'cancel_requested' WHERE id = ?")
        .bind(&intent.job_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    f.pipeline
        .tick_job(f.settings.clone(), &intent.id)
        .await
        .unwrap();
    let events = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(events.len(), 4);
    assert_ne!(events[2].1, old_worker);
    assert_eq!(events[2].1, events[3].1);
    assert_eq!(events[3].0, "acquisition_claim_returned");
    assert_eq!(
        f.pipeline
            .review("owner", &intent.id)
            .await
            .unwrap()
            .lifecycle,
        ReviewLifecycle::Unproven
    );
    release.notify_one();
    worker.await.unwrap().unwrap();
    let events = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(events.len(), 5);
    assert_eq!(events[4], ("acquisition_claim_returned".into(), old_worker));
    assert_eq!(
        f.pipeline
            .review("owner", &intent.id)
            .await
            .unwrap()
            .lifecycle,
        ReviewLifecycle::Returned
    );
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn lifecycle_aborted_work_stays_unmatched_across_reopen() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("aborted-lifecycle").await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut seen = f.server.seen.lock().unwrap();
        seen.payload_started = Some(started.clone());
        seen.payload_release = Some(release.clone());
    }
    mock::allow_search(&f.store).await;
    let pipeline = f.pipeline.clone();
    let settings = f.settings.clone();
    let id = intent.id.clone();
    let worker = tokio::spawn(async move { pipeline.tick_job(settings, &id).await });
    started.notified().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    release.notify_one();
    let before = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(before.len(), 2);
    assert_eq!(before[1].0, "acquisition_claim_started");
    f.store.close().await;
    let store = SqliteStore::open(f.dir.path()).await.unwrap();
    assert_eq!(lifecycle_events(&store, &intent.job_id).await, before);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn lifecycle_historical_acquisition_is_not_marked_covered() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("historical-lifecycle").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("DELETE FROM job_events WHERE job_id = ? AND event = 'acquisition_lifecycle_v1'")
        .bind(&intent.job_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    f.submit(&intent.id).await;
    let events = lifecycle_events(&f.store, &intent.job_id).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "acquisition_claim_started");
    assert_eq!(events[1].0, "acquisition_claim_returned");
    assert_eq!(events[0].1, events[1].1);
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
        let operation = ImportService::new(f.store.clone())
            .get(final_view.import_id.as_ref().unwrap())
            .await
            .unwrap();
        assert_eq!(operation.request.policy, ImportPolicy::Copy);
        assert_eq!(operation.effective_policy.as_deref(), Some("copy"));
        assert_eq!(
            operation.identity.mode,
            if torrent {
                ImportMode::TorrentPreserving
            } else {
                ImportMode::Standalone
            }
        );
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
            "/api/v1/acquisition/unused/correct-mapping",
            serde_json::json!({"revision":1,"polling_fingerprint":"unused","source_root_id":f.source_id,"source_relative_path":"owned.cbz"}),
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

impl Fixture {
    async fn expired_selection(&self, id: &str) -> AcquisitionReview {
        let mut tx = self.store.begin_write().await.unwrap();
        sqlx::query("UPDATE release_assessments SET created_at = 0, expires_at = 1 WHERE id = (SELECT assessment_id FROM release_decisions WHERE id = ?)")
            .bind(&self.request.selection_decision_id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        self.pipeline
            .tick_job(self.settings.clone(), id)
            .await
            .unwrap();
        let review = self.pipeline.review("owner", id).await.unwrap();
        assert_eq!(
            review.acquisition.reason,
            Some(AcquisitionReason::SelectionReviewRequired)
        );
        assert!(!review.acquisition.submitted);
        assert_eq!(review.lifecycle, ReviewLifecycle::Returned);
        review
    }

    async fn reassessment_request(&self, review: &AcquisitionReview) -> ReassessSelection {
        let assessment = Search::new(self.store.clone(), self.settings.clone())
            .assess_release(
                "owner",
                search::ReleaseAssessmentRequest {
                    unit_id: self.request.unit_id.clone(),
                    release_handle: self.request.release_handle.clone(),
                },
            )
            .await
            .unwrap();
        ReassessSelection {
            revision: review.revision,
            polling_fingerprint: review.polling_fingerprint.clone(),
            acknowledged_assessment_id: (assessment.evaluation.eligibility == Eligibility::Unknown)
                .then_some(assessment.assessment_id),
        }
    }
}

#[tokio::test]
async fn reassessment_retires_only_unsubmitted_intent_and_normal_creation_survives_reopen() {
    for (torrent, stale_target) in [(false, false), (true, false), (false, true)] {
        let f = Fixture::new(ContentType::Comic, torrent).await;
        let intent = f.create("expired-selection").await;
        let review = if stale_target {
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query("UPDATE editions SET region = 'US'")
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            f.pipeline
                .tick_job(f.settings.clone(), &intent.id)
                .await
                .unwrap();
            f.pipeline.review("owner", &intent.id).await.unwrap()
        } else {
            f.expired_selection(&intent.id).await
        };
        let request = f.reassessment_request(&review).await;
        let requests = f.server.seen.lock().unwrap().requests.clone();
        let result = f
            .pipeline
            .reassess_selection(&f.settings, "owner", &intent.id, request)
            .await
            .unwrap();
        assert_eq!(f.server.seen.lock().unwrap().requests, requests);
        assert_eq!(result.acquisition.state, AcquisitionState::Canceled);
        assert!(!result.acquisition.submitted);
        assert_eq!(result.acquisition.receipt_count, 0);
        assert_eq!(result.release_handle, f.request.release_handle);
        assert_ne!(
            Some(&result.decision.id),
            f.request.selection_decision_id.as_ref()
        );
        let old: String = sqlx::query_scalar("SELECT json_extract(request, '$.selection_decision_id') FROM acquisition_intents WHERE id = ?")
            .bind(&intent.id).fetch_one(f.store.reader()).await.unwrap();
        assert_eq!(Some(old), f.request.selection_decision_id);
        let audited: String = sqlx::query_scalar("SELECT reason FROM job_events WHERE job_id = ? AND event = 'acquisition_selection_reassessed'")
            .bind(&intent.job_id).fetch_one(f.store.reader()).await.unwrap();
        assert_eq!(audited, result.decision.id);
        let store = SqliteStore::open(f.dir.path()).await.unwrap();
        let pipeline = Pipeline::new(store.clone());
        let settings = Settings::new(
            store,
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        );
        let replay = pipeline
            .create(&settings, "owner", "expired-selection", f.request.clone())
            .await
            .unwrap();
        assert_eq!(replay.id, intent.id);
        assert_eq!(replay.state, AcquisitionState::Canceled);
        assert!(matches!(
            pipeline
                .reassess_selection(
                    &settings,
                    "owner",
                    &intent.id,
                    f.reassessment_request(&review).await
                )
                .await,
            Err(PipelineError::Conflict)
        ));
        let mut fresh = f.request.clone();
        fresh.selection_decision_id = Some(result.decision.id);
        let replacement = pipeline
            .create(&settings, "owner", "fresh-selection", fresh.clone())
            .await
            .unwrap();
        f.server.seen.lock().unwrap().own_tag = format!("libraryd-{}", replacement.id);
        mock::allow_search(&f.store).await;
        pipeline
            .tick_job(settings.clone(), &replacement.id)
            .await
            .unwrap();
        assert!(
            pipeline
                .get("owner", &replacement.id)
                .await
                .unwrap()
                .submitted
        );
        let submissions = f.server.seen.lock().unwrap().submissions;
        assert!(submissions > 0);
        assert_eq!(
            pipeline
                .create(&settings, "owner", "fresh-selection", fresh)
                .await
                .unwrap()
                .id,
            replacement.id
        );
        pipeline.tick_job(settings, &intent.id).await.unwrap();
        assert_eq!(submissions, f.server.seen.lock().unwrap().submissions);
    }
}

#[tokio::test]
async fn reassessment_fences_stale_revision_races_and_unauthorized_owner() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("reassessment-fences").await;
    let review = f.expired_selection(&intent.id).await;
    assert!(matches!(
        f.pipeline
            .reassess_selection(
                &f.settings,
                "other",
                &intent.id,
                f.reassessment_request(&review).await
            )
            .await,
        Err(PipelineError::NotFound)
    ));
    let mut stale = f.reassessment_request(&review).await;
    stale.revision -= 1;
    assert!(matches!(
        f.pipeline
            .reassess_selection(&f.settings, "owner", &intent.id, stale)
            .await,
        Err(PipelineError::Conflict)
    ));
    let original = f.reassessment_request(&review).await;
    let validated = f
        .pipeline
        .prepare_reassessment(&f.settings, "owner", &intent.id, &original)
        .await
        .unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    lifecycle_event(&mut tx, &intent.id, "snapshot_changed", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let review = f.pipeline.review("owner", &intent.id).await.unwrap();
    let updated = f.reassessment_request(&review).await;
    assert!(matches!(
        f.pipeline
            .apply_reassessment(&f.settings, "owner", &intent.id, &updated, validated)
            .await,
        Err(PipelineError::Conflict)
    ));
    let a = f.reassessment_request(&review).await;
    let b = f.reassessment_request(&review).await;
    let (a, b) = tokio::join!(
        f.pipeline
            .reassess_selection(&f.settings, "owner", &intent.id, a),
        f.pipeline
            .reassess_selection(&f.settings, "owner", &intent.id, b),
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert!(matches!(a.err().or(b.err()), Some(PipelineError::Conflict)));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM job_events WHERE event = 'acquisition_selection_reassessed'",
    )
    .fetch_one(f.store.reader())
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn reassessment_revalidates_all_fences_after_preparation_without_audit_or_writes() {
    for change in [
        "CREATE TRIGGER ignore_reassessment BEFORE UPDATE ON acquisition_runs BEGIN SELECT RAISE(IGNORE); END",
        "CREATE TRIGGER ignore_reassessment BEFORE UPDATE ON jobs BEGIN SELECT RAISE(IGNORE); END",
        "INSERT INTO job_events(job_id,event,to_state,created_at) SELECT job_id,'changed','needs_review',unixepoch() FROM acquisition_intents WHERE id = ?",
        "UPDATE acquisition_runs SET attempted = 1, payload_digest = 'attempted' WHERE id = ?",
        "UPDATE acquisition_runs SET reason = 'uncertain_submission' WHERE id = ?",
        "INSERT INTO acquisition_receipts(acquisition_id,ordinal,external_id) VALUES(?,0,'SABnzbd_nzo_owned')",
        "UPDATE acquisition_runs SET payload_digest = 'uncertain' WHERE id = ?",
        "UPDATE acquisition_runs SET torrent_hash = 'uncertain' WHERE id = ?",
        "UPDATE acquisition_runs SET import_id = 'existing' WHERE id = ?",
        "UPDATE acquisition_runs SET source_root = destination_root, source_relative = 'owned.cbz' WHERE id = ?",
        "UPDATE acquisition_runs SET work_token = 'expired', work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET work_until = 0 WHERE id = ?",
        "UPDATE acquisition_runs SET work_token = 'active' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'completed' WHERE id = ?",
        "UPDATE acquisition_runs SET state = 'canceled' WHERE id = ?",
        "UPDATE acquisition_runs SET category = 'foreign' WHERE id = ?",
        "UPDATE acquisition_intents SET caller = 'other' WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request,'$.unit_id','other') WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request,'$.client_id','other') WHERE id = ?",
        "UPDATE acquisition_intents SET request = json_set(request,'$.release_handle','other') WHERE id = ?",
        "UPDATE jobs SET worker = 'expired', lease_until = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET state = 'cancel_requested' WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET reason = 'uncertain_submission' WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET lease_until = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE jobs SET retry_at = 0 WHERE id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_lifecycle_v1' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "DELETE FROM job_events WHERE event = 'acquisition_claim_returned' AND job_id = (SELECT job_id FROM acquisition_intents WHERE id = ?)",
        "UPDATE integrations SET label = 'changed' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET enabled = 0 WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET base_url = 'http://127.0.0.1:1' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET secret_nonce = zeroblob(24), secret_ciphertext = zeroblob(28) WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET base_url = 'http://127.0.0.1:1' WHERE id = (SELECT integration_id FROM search_releases WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?))",
        "UPDATE release_assessments SET created_at = 0, expires_at = 1 WHERE unit_id = (SELECT unit_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET options = '{\"category\":\"changed\"}' WHERE id = (SELECT client_id FROM acquisition_runs WHERE id = ?)",
        "UPDATE integrations SET label = 'changed' WHERE id = (SELECT integration_id FROM search_releases WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?))",
        "UPDATE search_releases SET evidence_json = '{}' WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?)",
        "UPDATE search_releases SET owner = 'other' WHERE handle = (SELECT release_handle FROM acquisition_runs WHERE id = ?)",
        "UPDATE units SET label = 'changed' WHERE id = (SELECT unit_id FROM acquisition_runs WHERE id = ?)",
        "INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase) SELECT 'journal',destination_root,destination_root,destination_relative,destination_path,unit_id,'{}','{}','planned' FROM acquisition_runs WHERE id = ?",
    ] {
        let f = Fixture::new(ContentType::Comic, false).await;
        let intent = f.create("reassessment-unsafe").await;
        let review = f.expired_selection(&intent.id).await;
        let request = f.reassessment_request(&review).await;
        let validated = f
            .pipeline
            .prepare_reassessment(&f.settings, "owner", &intent.id, &request)
            .await
            .unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        let mut tx = f.store.begin_write().await.unwrap();
        let query = sqlx::query(change);
        let query = if change.starts_with("CREATE TRIGGER") {
            query
        } else {
            query.bind(&intent.id)
        };
        query.execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert!(
            f.pipeline
                .apply_reassessment(&f.settings, "owner", &intent.id, &request, validated)
                .await
                .is_err(),
            "{change}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM release_decisions WHERE action = 'selected'")
                .fetch_one(f.store.reader())
                .await
                .unwrap();
        assert_eq!(count, 1, "{change}");
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM job_events WHERE event = 'acquisition_selection_reassessed'",
        )
        .fetch_one(f.store.reader())
        .await
        .unwrap();
        assert_eq!(count, 0, "{change}");
        if !change.contains("UPDATE units")
            && !change.contains("SELECT job_id,'changed'")
            && !change.contains("SET label = 'changed'")
        {
            // A newly reviewed unsafe state must also fail: the snapshot fence alone is insufficient.
            let current = f.pipeline.review("owner", &intent.id).await;
            if let Ok(current) = current {
                let current_request = ReassessSelection {
                    revision: current.revision,
                    polling_fingerprint: current.polling_fingerprint,
                    acknowledged_assessment_id: request.acknowledged_assessment_id.clone(),
                };
                assert!(
                    f.pipeline
                        .reassess_selection(&f.settings, "owner", &intent.id, current_request)
                        .await
                        .is_err(),
                    "fresh unsafe review: {change}"
                );
            }
        }
        assert_eq!(requests, f.server.seen.lock().unwrap().requests, "{change}");
    }
}

#[tokio::test]
async fn reassessment_requires_fresh_evidence_and_exact_acknowledgement() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("reassessment-freshness").await;
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE acquisition_runs SET state = 'needs_review', reason = 'selection_review_required' WHERE id = ?")
        .bind(&intent.id).execute(&mut *tx).await.unwrap();
    job_state(
        &mut tx,
        &intent.id,
        "needs_review",
        "fixture",
        Some("selection_review_required"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let review = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert!(matches!(
        f.pipeline
            .reassess_selection(
                &f.settings,
                "owner",
                &intent.id,
                f.reassessment_request(&review).await
            )
            .await,
        Err(PipelineError::SelectionCurrent)
    ));
    // A fresh selected decision must not silently replace an exact acknowledgement.
    let review = f.expired_selection(&intent.id).await;
    let mut request = f.reassessment_request(&review).await;
    assert!(request.acknowledged_assessment_id.is_some());
    request.acknowledged_assessment_id = None;
    assert!(matches!(
        f.pipeline
            .reassess_selection(&f.settings, "owner", &intent.id, request)
            .await,
        Err(PipelineError::Selection(
            SelectionError::AcknowledgementRequired
        ))
    ));
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE search_releases SET expires_at = 1 WHERE handle = ?")
        .bind(&f.request.release_handle)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let review = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert!(matches!(
        f.pipeline
            .reassess_selection(
                &f.settings,
                "owner",
                &intent.id,
                ReassessSelection {
                    revision: review.revision,
                    polling_fingerprint: review.polling_fingerprint,
                    acknowledged_assessment_id: None
                }
            )
            .await,
        Err(PipelineError::SelectionRefreshRequired)
    ));
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
    // Explicit search refreshes the same durable handle; reassessment itself never searches.
    let integration_id: String =
        sqlx::query_scalar("SELECT integration_id FROM search_releases WHERE handle = ?")
            .bind(&f.request.release_handle)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    mock::allow_search(&f.store).await;
    let refreshed = Search::new(f.store.clone(), f.settings.clone())
        .releases(
            "owner",
            ReleaseSearch {
                integration_id,
                content_type: ContentType::Comic,
                query: "Owned Fixture".into(),
                unit_id: Some(f.request.unit_id.clone()),
                offset: 0,
                limit: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        refreshed.releases[0].release_handle,
        f.request.release_handle
    );
    let review = f.pipeline.review("owner", &intent.id).await.unwrap();
    let requests = f.server.seen.lock().unwrap().requests.clone();
    f.pipeline
        .reassess_selection(
            &f.settings,
            "owner",
            &intent.id,
            f.reassessment_request(&review).await,
        )
        .await
        .unwrap();
    assert_eq!(requests, f.server.seen.lock().unwrap().requests);
}

#[tokio::test]
async fn reassessment_keeps_rejection_history_and_submission_rechecks() {
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("reassessment-rejection").await;
    let review = f.expired_selection(&intent.id).await;
    let request = f.reassessment_request(&review).await;
    let validated = f
        .pipeline
        .prepare_reassessment(&f.settings, "owner", &intent.id, &request)
        .await
        .unwrap();
    let assessment_id = validated.decision_request.assessment_id.clone();
    let repo = SelectionRepository::new(f.store.clone());
    let rejection = repo
        .decide(
            "owner",
            "reject-during-reassessment",
            NewDecision {
                assessment_id: assessment_id.clone(),
                action: DecisionAction::Rejected,
                acknowledged_assessment_id: None,
                reason: Some("wrong release".into()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.pipeline
            .apply_reassessment(&f.settings, "owner", &intent.id, &request, validated)
            .await,
        Err(PipelineError::Conflict | PipelineError::Selection(SelectionError::Rejected))
    ));
    let rejected = f.pipeline.review("owner", &intent.id).await.unwrap();
    assert!(matches!(
        f.pipeline
            .reassess_selection(
                &f.settings,
                "owner",
                &intent.id,
                f.reassessment_request(&rejected).await
            )
            .await,
        Err(PipelineError::Selection(SelectionError::Rejected))
    ));
    repo.revoke_rejection("owner", "revoke", &rejection.id)
        .await
        .unwrap();
    let review = f.pipeline.review("owner", &intent.id).await.unwrap();
    let result = f
        .pipeline
        .reassess_selection(
            &f.settings,
            "owner",
            &intent.id,
            f.reassessment_request(&review).await,
        )
        .await
        .unwrap();
    let superseded: bool =
        sqlx::query_scalar("SELECT superseded_by IS NOT NULL FROM release_decisions WHERE id = ?")
            .bind(&f.request.selection_decision_id)
            .fetch_one(f.store.reader())
            .await
            .unwrap();
    assert!(superseded);
    let mut fresh = f.request.clone();
    fresh.selection_decision_id = Some(result.decision.id.clone());
    let replacement = f
        .pipeline
        .create(&f.settings, "owner", "replacement", fresh)
        .await
        .unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE release_assessments SET created_at = 0, expires_at = 1 WHERE id = ?")
        .bind(&assessment_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    f.pipeline
        .tick_job(f.settings.clone(), &replacement.id)
        .await
        .unwrap();
    assert_eq!(
        f.pipeline
            .get("owner", &replacement.id)
            .await
            .unwrap()
            .reason,
        Some(AcquisitionReason::SelectionReviewRequired)
    );
    assert_eq!(f.server.seen.lock().unwrap().submissions, 0);
}

#[tokio::test]
async fn reassessment_http_returns_distinct_conflicts_and_safe_decision_without_queuing() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let f = Fixture::new(ContentType::Comic, false).await;
    let intent = f.create("reassessment-http").await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    let login = auth
        .setup("fixture-admin", "fixture-secure-password")
        .await
        .unwrap();
    let owner = login.principal.user_id;
    let key = auth
        .create_key("manager", crate::auth::Scope::Manage)
        .await
        .unwrap();
    let mut tx = f.store.begin_write().await.unwrap();
    job_state(
        &mut tx,
        &intent.id,
        "needs_review",
        "fixture",
        Some("selection_review_required"),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE acquisition_runs SET state = 'needs_review', reason = 'selection_review_required' WHERE id = ?")
        .bind(&intent.id).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE acquisition_intents SET caller = ?")
        .bind(&owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    for statement in [
        "UPDATE search_releases SET owner = ?",
        "UPDATE release_selection_policies SET owner = ?",
        "UPDATE release_assessments SET owner = ?",
        "UPDATE release_decisions SET owner = ?",
    ] {
        sqlx::query(statement)
            .bind(&owner)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let app =
        crate::httpapi::acquisition::routes(crate::httpapi::acquisition::AcquisitionContext {
            pipeline: f.pipeline.clone(),
            settings: f.settings.clone(),
            auth: crate::httpapi::auth::AuthContext {
                service: auth,
                origin: "http://localhost".into(),
            },
        });
    let mut ack = None;
    for expected in [
        "selection_current",
        "selection_refresh_required",
        "assessment_acknowledgement_required",
        "success",
        "acquisition_conflict",
    ] {
        if expected == "selection_refresh_required" {
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query("UPDATE release_assessments SET created_at = 0, expires_at = 1")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE search_releases SET expires_at = 1")
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        if expected == "assessment_acknowledgement_required" {
            let mut tx = f.store.begin_write().await.unwrap();
            sqlx::query("UPDATE search_releases SET expires_at = unixepoch() + 3600")
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        if expected == "success" {
            let assessed = Search::new(f.store.clone(), f.settings.clone())
                .assess_release(
                    &owner,
                    search::ReleaseAssessmentRequest {
                        release_handle: f.request.release_handle.clone(),
                        unit_id: f.request.unit_id.clone(),
                    },
                )
                .await
                .unwrap();
            ack = Some(assessed.assessment_id);
        }
        let review = f.pipeline.review(&owner, &intent.id).await.unwrap();
        let requests = f.server.seen.lock().unwrap().requests.clone();
        let response = app.clone().oneshot(Request::builder()
            .method("POST").uri(format!("/api/v1/acquisition/{}/reassess-selection",intent.id))
            .header("authorization",format!("Bearer {}",key.secret)).header("content-type","application/json")
            .body(Body::from(serde_json::json!({"revision":review.revision,"polling_fingerprint":review.polling_fingerprint,"acknowledged_assessment_id":ack}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        if expected == "success" {
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        if expected == "success" {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["acquisition"]["state"], "canceled");
            assert_eq!(body["acquisition"]["submitted"], false);
            assert_eq!(body["release_handle"], f.request.release_handle);
            assert_eq!(body["decision"]["action"], "selected");
            for private in ["base_url", "external_id", "api_key", "selected.cbz"] {
                assert!(!body.to_string().contains(private));
            }
        } else {
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"]["code"], expected);
        }
        assert_eq!(requests, f.server.seen.lock().unwrap().requests);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(f.store.reader())
        .await
        .unwrap();
    assert_eq!(count, 1);
}
