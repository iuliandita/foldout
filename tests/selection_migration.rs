use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn cached_evidence_upgrade_preserves_legacy_handles_without_inventing_evidence() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut connection)
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations.iter().filter(|migration| migration.version < 16) {
        sqlx::raw_sql(migration.sql.clone())
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO integrations VALUES ('source','prowlarr','Source','https://example.invalid/',1,'{}',1,zeroblob(24),zeroblob(28))")
        .execute(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO search_releases VALUES ('release','owner','source','fingerprint',1,'digest','comic','usenet','query',0,20,100)")
        .execute(&mut connection).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/016_cached_release_evidence.sql"
    ))
    .execute(&mut connection)
    .await
    .unwrap();
    let evidence: Option<String> =
        sqlx::query_scalar("SELECT evidence_json FROM search_releases WHERE handle='release'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(evidence, None);
    for invalid in [
        "not-json".to_owned(),
        "[]".to_owned(),
        "null".to_owned(),
        format!("{{\"title\":\"{}\"}}", "a".repeat(32768)),
    ] {
        assert!(
            sqlx::query("UPDATE search_releases SET evidence_json = ? WHERE handle='release'")
                .bind(invalid)
                .execute(&mut connection)
                .await
                .is_err()
        );
    }
    sqlx::query("UPDATE search_releases SET evidence_json = '{}' WHERE handle='release'")
        .execute(&mut connection)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn decision_supersession_upgrade_fails_closed_for_earlier_selections() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut connection)
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations.iter().filter(|migration| migration.version < 17) {
        sqlx::raw_sql(migration.sql.clone())
            .execute(&mut connection)
            .await
            .unwrap();
    }
    let digest = |c: char| c.to_string().repeat(64);
    sqlx::raw_sql(
        "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p','comic','T','T');
         INSERT INTO editions(id,publication_id,language) VALUES('e','p','en');
         INSERT INTO units(id,edition_id,label,kind) VALUES('u','e','1','issue');",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query("INSERT INTO release_selection_policies(id,owner,unit_id,revision,mode,fingerprint) VALUES('policy','owner','u',1,'review_only',?)")
        .bind(digest('a')).execute(&mut connection).await.unwrap();
    for (id, guid) in [("same", 'b'), ("same-again", 'b'), ("other", 'c')] {
        sqlx::query("INSERT INTO release_assessments(id,owner,unit_id,release_handle,integration_id,source_fingerprint,indexer_id,guid_digest,content_type,protocol,policy_id,policy_fingerprint,target_fingerprint,evidence_fingerprint,evidence_json,reasons_json,eligibility,created_at,expires_at) VALUES(?,'owner','u',?,'source',?,1,?,'comic','usenet','policy',?,?,?,'{}','[]','eligible',1,2)")
            .bind(id).bind(format!("handle-{id}")).bind(digest('d')).bind(digest(guid)).bind(digest('a')).bind(digest('e')).bind(digest('f'))
            .execute(&mut connection).await.unwrap();
    }
    for (id, assessment, action, created_at) in [
        ("before", "same", "selected", 100),
        ("tie", "same", "selected", 200),
        ("rejection", "same-again", "rejected", 200),
        ("after", "same", "selected", 300),
        ("unrelated", "other", "selected", 100),
    ] {
        sqlx::query("INSERT INTO release_decisions(id,owner,assessment_id,action,idempotency_key,request_fingerprint,created_at) VALUES(?,'owner',?,?,?,?,?)")
            .bind(id).bind(assessment).bind(action).bind(id).bind(digest('0')).bind(created_at)
            .execute(&mut connection).await.unwrap();
    }
    sqlx::query("INSERT INTO release_rejection_revocations(id,owner,rejection_decision_id,idempotency_key,request_fingerprint) VALUES('revocation','owner','rejection','revoke',?)")
        .bind(digest('0')).execute(&mut connection).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/017_release_decision_supersession.sql"
    ))
    .execute(&mut connection)
    .await
    .unwrap();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, superseded_by FROM release_decisions ORDER BY id")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("after".into(), None),
            ("before".into(), Some("rejection".into())),
            ("rejection".into(), None),
            ("tie".into(), Some("rejection".into())),
            ("unrelated".into(), None),
        ]
    );
    assert!(
        sqlx::query("UPDATE release_decisions SET superseded_by = 'after' WHERE id = 'rejection'")
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        0
    );
}
