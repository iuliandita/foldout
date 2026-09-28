use super::*;
use crate::{
    catalog::wanted::WantedRepository, providers::release_evidence::parse_release_evidence,
};

fn identity() -> ReleaseIdentity {
    ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        integration_id: Uuid::new_v4().to_string(),
        source_fingerprint: "a".repeat(64),
        indexer_id: 7,
        guid_digest: "b".repeat(64),
        content_type: ContentType::Magazine,
        protocol: ReleaseProtocol::Usenet,
    }
}

async fn assess(
    repository: &SelectionRepository,
    store: &SqliteStore,
    owner: &str,
    unit: &str,
    identity: ReleaseIdentity,
    title: &str,
) -> Assessment {
    let policy = repository
        .current_or_default_policy(owner, unit)
        .await
        .unwrap();
    let target = WantedRepository::new(store.clone())
        .unit_context(unit)
        .await
        .unwrap()
        .unwrap();
    let evidence = parse_release_evidence(title, ContentType::Magazine);
    let evaluation = evaluate(
        &target,
        &Candidate {
            source: &identity.integration_id,
            content_type: Evidence::Unknown,
            evidence: &evidence,
        },
    );
    repository
        .record_assessment(
            owner,
            AssessmentInput {
                target: &target,
                policy_id: &policy.id,
                identity,
                evidence: &evidence,
                evaluation: &evaluation,
                expires_at: now() + 300,
            },
        )
        .await
        .unwrap()
}

fn select(assessment: &Assessment) -> NewDecision {
    NewDecision {
        assessment_id: assessment.id.clone(),
        action: DecisionAction::Selected,
        acknowledged_assessment_id: Some(assessment.id.clone()),
        reason: None,
    }
}

fn reject(assessment: &Assessment) -> NewDecision {
    NewDecision {
        assessment_id: assessment.id.clone(),
        action: DecisionAction::Rejected,
        acknowledged_assessment_id: None,
        reason: Some("Wrong edition".into()),
    }
}

async fn expire(store: &SqliteStore, id: &str) {
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE release_assessments SET created_at = unixepoch()-100, expires_at = unixepoch()-1 WHERE id = ?").bind(id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn acknowledgement_is_exact_and_replay_survives_expiry_and_policy_change() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let assessment = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    for ack in [None, Some(Uuid::new_v4().to_string())] {
        assert!(matches!(
            repository
                .decide(
                    "owner",
                    "missing-ack",
                    NewDecision {
                        acknowledged_assessment_id: ack,
                        ..select(&assessment)
                    }
                )
                .await,
            Err(SelectionError::AcknowledgementRequired)
        ));
    }
    let (first, repeat) = tokio::join!(
        repository.decide("owner", "choose", select(&assessment)),
        repository.decide("owner", "choose", select(&assessment))
    );
    let first = first.unwrap();
    assert_eq!(first, repeat.unwrap());
    expire(&store, &assessment.id).await;
    repository
        .append_policy(
            "owner",
            &unit,
            1,
            PolicyDraft {
                format_order: vec![MediaFormat::Pdf],
                source_priority: vec![],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        repository
            .decide("owner", "choose", select(&assessment))
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        repository
            .decide("owner", "choose", reject(&assessment))
            .await,
        Err(SelectionError::Conflict)
    ));
    assert!(matches!(
        repository
            .decide("owner", "new-key", select(&assessment))
            .await,
        Err(SelectionError::Expired)
    ));
}

#[tokio::test]
async fn explicit_conflicts_cannot_be_selected_and_ownership_is_enforced() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let assessment = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #99 [language:en] [pdf]",
    )
    .await;
    assert!(matches!(
        repository
            .decide("owner", "choose", select(&assessment))
            .await,
        Err(SelectionError::Ineligible)
    ));
    assert!(matches!(
        repository
            .decide("other", "reject", reject(&assessment))
            .await,
        Err(SelectionError::NotFound)
    ));
    let rejected = repository
        .decide("owner", "reject", reject(&assessment))
        .await
        .unwrap();
    assert!(matches!(
        repository
            .revoke_rejection("other", "revoke", &rejected.id)
            .await,
        Err(SelectionError::NotFound)
    ));
    assert!(matches!(
        repository.active_rejection("other", &assessment.id).await,
        Err(SelectionError::NotFound)
    ));
}

#[tokio::test]
async fn rejection_survives_handle_and_configuration_changes_and_policy_reversion() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let release = identity();
    let a = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let rejected = repository
        .decide("owner", "reject", reject(&a))
        .await
        .unwrap();
    expire(&store, &a.id).await;
    let release = ReleaseIdentity {
        handle: Uuid::new_v4().to_string(),
        source_fingerprint: "c".repeat(64),
        ..release
    };
    let fresh = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    assert_eq!(
        repository
            .active_rejection("owner", &fresh.id)
            .await
            .unwrap(),
        Some(rejected.clone())
    );
    assert!(matches!(
        repository.decide("owner", "choose", select(&fresh)).await,
        Err(SelectionError::Rejected)
    ));
    repository
        .append_policy(
            "owner",
            &unit,
            1,
            PolicyDraft {
                format_order: vec![MediaFormat::Pdf],
                source_priority: vec![],
            },
        )
        .await
        .unwrap();
    let b = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    assert!(
        repository
            .active_rejection("owner", &b.id)
            .await
            .unwrap()
            .is_none()
    );
    repository
        .append_policy("owner", &unit, 2, PolicyDraft::default())
        .await
        .unwrap();
    let a_again = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release,
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    assert_eq!(
        repository
            .active_rejection("owner", &a_again.id)
            .await
            .unwrap(),
        Some(rejected)
    );
}

#[tokio::test]
async fn rejection_scope_does_not_cross_owner_unit_or_release_identity() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let release = identity();
    let first = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    repository
        .decide("owner", "reject", reject(&first))
        .await
        .unwrap();
    let other = assess(
        &repository,
        &store,
        "other",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    assert!(
        repository
            .active_rejection("other", &other.id)
            .await
            .unwrap()
            .is_none()
    );
    let other_unit = Uuid::new_v4().to_string();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO units(id,edition_id,label,kind) SELECT ?,edition_id,'2',kind FROM units WHERE id = ?").bind(&other_unit).bind(&unit).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let other = assess(
        &repository,
        &store,
        "owner",
        &other_unit,
        release.clone(),
        "Monthly #2 [language:en] [pdf]",
    )
    .await;
    assert!(
        repository
            .active_rejection("owner", &other.id)
            .await
            .unwrap()
            .is_none()
    );
    for release in [
        ReleaseIdentity {
            guid_digest: "d".repeat(64),
            ..release.clone()
        },
        ReleaseIdentity {
            integration_id: Uuid::new_v4().to_string(),
            ..release.clone()
        },
        ReleaseIdentity {
            indexer_id: 8,
            ..release.clone()
        },
        ReleaseIdentity {
            protocol: ReleaseProtocol::Torrent,
            ..release
        },
    ] {
        let other = assess(
            &repository,
            &store,
            "owner",
            &unit,
            release,
            "Monthly #1 [language:en] [pdf]",
        )
        .await;
        assert!(
            repository
                .active_rejection("owner", &other.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn revocation_is_explicit_immutable_and_idempotent() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let assessment = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let rejected = repository
        .decide("owner", "reject", reject(&assessment))
        .await
        .unwrap();
    assert!(matches!(
        repository
            .decide("owner", "second-rejection", reject(&assessment))
            .await,
        Err(SelectionError::Conflict)
    ));
    let revoked = repository
        .revoke_rejection("owner", "revoke", &rejected.id)
        .await
        .unwrap();
    assert_eq!(
        repository
            .revoke_rejection("owner", "revoke", &rejected.id)
            .await
            .unwrap(),
        revoked
    );
    assert!(matches!(
        repository
            .revoke_rejection("owner", "revoke", &Uuid::new_v4().to_string())
            .await,
        Err(SelectionError::Conflict)
    ));
    assert!(matches!(
        repository
            .revoke_rejection("owner", "another-key", &rejected.id)
            .await,
        Err(SelectionError::Conflict)
    ));
    assert!(
        repository
            .active_rejection("owner", &assessment.id)
            .await
            .unwrap()
            .is_none()
    );
    let selected = repository
        .decide("owner", "choose", select(&assessment))
        .await
        .unwrap();
    assert!(matches!(
        repository
            .revoke_rejection("owner", "wrong-action", &selected.id)
            .await,
        Err(SelectionError::Invalid)
    ));
    assert_eq!(
        repository
            .decide("owner", "reject", reject(&assessment))
            .await
            .unwrap(),
        rejected
    );
}

#[tokio::test]
async fn cleanup_is_bounded_and_preserves_decision_history() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let kept = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    repository
        .decide("owner", "reject", reject(&kept))
        .await
        .unwrap();
    let discarded = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    expire(&store, &kept.id).await;
    expire(&store, &discarded.id).await;
    assert_eq!(repository.cleanup_expired(1).await.unwrap(), 1);
    assert_eq!(repository.cleanup_expired(1).await.unwrap(), 0);
    assert!(
        repository
            .active_rejection("owner", &kept.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        repository.active_rejection("owner", &discarded.id).await,
        Err(SelectionError::NotFound)
    ));
}

#[tokio::test]
async fn new_decisions_recheck_policy_and_target() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let a = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    repository
        .append_policy(
            "owner",
            &unit,
            1,
            PolicyDraft {
                format_order: vec![MediaFormat::Pdf],
                source_priority: vec![],
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        repository.decide("owner", "old-policy", select(&a)).await,
        Err(SelectionError::Changed)
    ));
    let b = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE units SET label = '2' WHERE id = ?")
        .bind(&unit)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        repository.decide("owner", "old-target", select(&b)).await,
        Err(SelectionError::Changed)
    ));
}

#[tokio::test]
async fn rejection_permanently_supersedes_earlier_selections_in_scope() {
    let (_dir, store, repository, unit) = super::tests::fixture().await;
    let release = identity();
    let first = assess(
        &repository,
        &store,
        "owner",
        &unit,
        release.clone(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let other_release = assess(
        &repository,
        &store,
        "owner",
        &unit,
        identity(),
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let old = repository
        .decide("owner", "old-select", select(&first))
        .await
        .unwrap();
    let unrelated = repository
        .decide("owner", "other-select", select(&other_release))
        .await
        .unwrap();
    let again = assess(
        &repository,
        &store,
        "owner",
        &unit,
        ReleaseIdentity {
            handle: Uuid::new_v4().to_string(),
            ..release
        },
        "Monthly #1 [language:en] [pdf]",
    )
    .await;
    let rejection = repository
        .decide("owner", "reject", reject(&again))
        .await
        .unwrap();
    repository
        .revoke_rejection("owner", "revoke", &rejection.id)
        .await
        .unwrap();
    let fresh = repository
        .decide("owner", "fresh-select", select(&again))
        .await
        .unwrap();
    let superseded = |id: String| {
        let store = store.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT superseded_by FROM release_decisions WHERE id = ?",
            )
            .bind(id)
            .fetch_one(store.reader())
            .await
            .unwrap()
        }
    };
    assert_eq!(superseded(old.id.clone()).await, Some(rejection.id.clone()));
    assert_eq!(superseded(unrelated.id).await, None);
    assert_eq!(superseded(fresh.id).await, None);
    assert_eq!(
        repository
            .decide("owner", "old-select", select(&first))
            .await
            .unwrap(),
        old
    );
}
