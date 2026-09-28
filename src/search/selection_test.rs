use super::*;
use crate::catalog::{
    CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind,
};
use crate::{
    catalog::wanted::WantedRepository,
    providers::{
        ReleaseProtocol,
        release_evidence::{Evidence, parse_release_evidence},
    },
    search::matching::{Candidate, evaluate},
};

pub(super) async fn fixture() -> (tempfile::TempDir, SqliteStore, SelectionRepository, String) {
    let dir = crate::search::mock::private_directory();
    let store = SqliteStore::open(dir.path()).await.unwrap();
    let catalog = CatalogRepository::new(store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type: ContentType::Magazine,
            title: "Monthly".into(),
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
    let unit = catalog
        .create_unit(NewUnit {
            edition_id: edition.id,
            label: "1".into(),
            kind: UnitKind::Issue,
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    let repository = SelectionRepository::new(store.clone());
    (dir, store, repository, unit.id)
}

#[tokio::test]
async fn concurrent_defaults_are_empty_and_owner_scoped() {
    let (_dir, store, repository, unit) = fixture().await;
    let (first, second) = tokio::join!(
        repository.current_or_default_policy("owner", &unit),
        repository.current_or_default_policy("owner", &unit),
    );
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert_eq!(first.revision, 1);
    assert_eq!(first.preferences, PolicyDraft::default());
    let other = repository
        .current_or_default_policy("other", &unit)
        .await
        .unwrap();
    assert_ne!(first.id, other.id);
    assert_eq!(first.fingerprint, other.fingerprint);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_selection_policies")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn reverting_policy_appends_history_with_original_fingerprint() {
    let (_dir, store, repository, unit) = fixture().await;
    let first = repository
        .current_or_default_policy("owner", &unit)
        .await
        .unwrap();
    let draft = PolicyDraft {
        format_order: vec![MediaFormat::Pdf, MediaFormat::Cbz],
        source_priority: vec![],
    };
    let second = repository
        .append_policy("owner", &unit, 1, draft.clone())
        .await
        .unwrap();
    assert_eq!(second.revision, 2);
    assert_ne!(first.fingerprint, second.fingerprint);
    assert!(matches!(
        repository
            .append_policy("owner", &unit, 1, draft.clone())
            .await,
        Err(SelectionError::Changed)
    ));
    assert_eq!(
        repository
            .append_policy("owner", &unit, 2, draft)
            .await
            .unwrap(),
        second
    );
    let third = repository
        .append_policy("owner", &unit, 2, PolicyDraft::default())
        .await
        .unwrap();
    assert_eq!(third.revision, 3);
    assert_eq!(third.fingerprint, first.fingerprint);
    assert_ne!(third.id, first.id);
    let rows = sqlx::query(
        "SELECT * FROM release_selection_policies WHERE owner = 'owner' ORDER BY revision",
    )
    .fetch_all(store.reader())
    .await
    .unwrap();
    let saved: Vec<_> = rows
        .into_iter()
        .map(|row| policy_row(row).unwrap())
        .collect();
    assert_eq!(saved, vec![first, second, third]);
}

#[tokio::test]
async fn invalid_and_missing_scopes_never_create_policy_rows() {
    let (_dir, store, repository, unit) = fixture().await;
    assert!(matches!(
        repository
            .current_or_default_policy("bad owner", &unit)
            .await,
        Err(SelectionError::Invalid)
    ));
    assert!(matches!(
        repository
            .current_or_default_policy("owner", "bad-id")
            .await,
        Err(SelectionError::Invalid)
    ));
    assert!(matches!(
        repository
            .current_or_default_policy("owner", &Uuid::nil().to_string())
            .await,
        Err(SelectionError::Invalid)
    ));
    assert!(matches!(
        repository
            .current_or_default_policy("owner", &Uuid::new_v4().to_string())
            .await,
        Err(SelectionError::NotFound)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_selection_policies")
        .fetch_one(store.reader())
        .await
        .unwrap();
    assert_eq!(count, 0);
    repository
        .current_or_default_policy("owner", &unit)
        .await
        .unwrap();
    for draft in [
        PolicyDraft {
            format_order: vec![MediaFormat::Pdf, MediaFormat::Pdf],
            source_priority: vec![],
        },
        PolicyDraft {
            format_order: vec![],
            source_priority: vec!["not-an-id".into()],
        },
        PolicyDraft {
            format_order: vec![],
            source_priority: vec![unit.clone(), unit.clone()],
        },
        PolicyDraft {
            format_order: vec![],
            source_priority: vec![Uuid::nil().to_string()],
        },
    ] {
        assert!(matches!(
            repository.append_policy("owner", &unit, 1, draft).await,
            Err(SelectionError::Invalid)
        ));
    }
    assert_eq!(
        repository
            .current_or_default_policy("owner", &unit)
            .await
            .unwrap()
            .revision,
        1
    );
}

#[tokio::test]
async fn assessment_dedup_is_exact_and_handle_sensitive() {
    let (_dir, store, repository, unit) = fixture().await;
    let policy = repository
        .current_or_default_policy("owner", &unit)
        .await
        .unwrap();
    let target = WantedRepository::new(store.clone())
        .unit_context(&unit)
        .await
        .unwrap()
        .unwrap();
    let evidence = parse_release_evidence("Monthly #1 [language:en] [cbz]", ContentType::Magazine);
    let identity = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        integration_id: Uuid::new_v4().to_string(),
        source_fingerprint: "a".repeat(64),
        indexer_id: 7,
        guid_digest: "b".repeat(64),
        content_type: ContentType::Magazine,
        protocol: ReleaseProtocol::Usenet,
    };
    let evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &evidence,
        },
    );
    let first = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    let repeat = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    assert_eq!(first, repeat);
    let changed = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        ..identity
    };
    let fresh = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: changed,
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    assert_ne!(first.id, fresh.id);
}

#[tokio::test]
async fn changed_evidence_is_fresh_and_spoofs_or_scope_mismatches_are_rejected() {
    let (_dir, store, repository, unit) = fixture().await;
    let policy = repository
        .current_or_default_policy("owner", &unit)
        .await
        .unwrap();
    let target = WantedRepository::new(store)
        .unit_context(&unit)
        .await
        .unwrap()
        .unwrap();
    let evidence = parse_release_evidence("Monthly #1 [language:en] [cbz]", ContentType::Magazine);
    let identity = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        integration_id: Uuid::new_v4().to_string(),
        source_fingerprint: "a".repeat(64),
        indexer_id: 7,
        guid_digest: "b".repeat(64),
        content_type: ContentType::Magazine,
        protocol: ReleaseProtocol::Usenet,
    };
    let evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &evidence,
        },
    );
    let first = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    let changed_evidence =
        parse_release_evidence("Other #1 [language:en] [cbz]", ContentType::Magazine);
    let changed_evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &changed_evidence,
        },
    );
    let fresh = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &changed_evidence,
                evaluation: &changed_evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    assert_ne!(first.id, fresh.id);
    let spoof = Evaluation {
        eligibility: crate::search::matching::Eligibility::Eligible,
        reasons: vec![],
    };
    assert!(matches!(
        repository
            .record_assessment(
                "owner",
                AssessmentInput {
                    target: &target,
                    policy_id: &policy.id,
                    identity: identity.clone(),
                    evidence: &evidence,
                    evaluation: &spoof,
                    expires_at: now() + 300
                }
            )
            .await,
        Err(SelectionError::Invalid)
    ));
    let mismatched = ReleaseIdentity {
        content_type: ContentType::Comic,
        ..identity
    };
    assert!(matches!(
        repository
            .record_assessment(
                "owner",
                AssessmentInput {
                    target: &target,
                    policy_id: &policy.id,
                    identity: mismatched,
                    evidence: &evidence,
                    evaluation: &evaluation,
                    expires_at: now() + 300
                }
            )
            .await,
        Err(SelectionError::Invalid)
    ));
}

#[tokio::test]
async fn stale_policy_or_target_and_expired_assessment_create_no_replay() {
    let (_dir, store, repository, unit) = fixture().await;
    let policy = repository
        .current_or_default_policy("owner", &unit)
        .await
        .unwrap();
    let target = WantedRepository::new(store.clone())
        .unit_context(&unit)
        .await
        .unwrap()
        .unwrap();
    let evidence = parse_release_evidence("Monthly #1 [language:en] [cbz]", ContentType::Magazine);
    let identity = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        integration_id: Uuid::new_v4().to_string(),
        source_fingerprint: "a".repeat(64),
        indexer_id: 7,
        guid_digest: "b".repeat(64),
        content_type: ContentType::Magazine,
        protocol: ReleaseProtocol::Usenet,
    };
    let evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &evidence,
        },
    );
    let first = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE release_assessments SET created_at = 0, expires_at = 1 WHERE id = ?")
        .bind(&first.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let fresh = repository
        .record_assessment(
            "owner",
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity: identity.clone(),
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap();
    assert_ne!(first.id, fresh.id);
    let next = repository
        .append_policy(
            "owner",
            &unit,
            policy.revision,
            PolicyDraft {
                format_order: vec![MediaFormat::Cbz],
                source_priority: vec![],
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        repository
            .record_assessment(
                "owner",
                AssessmentInput {
                    target: &target,
                    policy_id: &policy.id,
                    identity: identity.clone(),
                    evidence: &evidence,
                    evaluation: &evaluation,
                    expires_at: now() + 300
                }
            )
            .await,
        Err(SelectionError::Changed)
    ));
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE publications SET title = 'Changed' WHERE id = ?")
        .bind(&target.publication.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        repository
            .record_assessment(
                "owner",
                AssessmentInput {
                    target: &target,
                    policy_id: &next.id,
                    identity,
                    evidence: &evidence,
                    evaluation: &evaluation,
                    expires_at: now() + 300
                }
            )
            .await,
        Err(SelectionError::Changed)
    ));
}
