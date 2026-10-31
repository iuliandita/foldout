use libraryd::{
    backup,
    settings::{CreateIntegration, EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{fs, os::unix::fs::PermissionsExt};

#[tokio::test]
async fn direct_checkpoint_upgrade_preserves_fences_and_backup_rollback_schema() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let database = state.join("library.sqlite3");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&database)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    let old = sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .filter(|m| m.version < 24)
            .cloned()
            .collect(),
    );
    old.run(&mut db).await.unwrap();
    let fixture = root.path().join("settings-fixture");
    let key = EncryptionKey::load_or_create(&fixture).await.unwrap();
    let source = SqliteStore::open(&fixture).await.unwrap();
    Settings::new(source.clone(), key).create(serde_json::from_value::<CreateIntegration>(serde_json::json!({
        "kind":"mangadex", "label":"Owned", "base_url":"https://example.invalid", "enabled":true
    })).unwrap()).await.unwrap();
    let secret = sqlx::query("SELECT id,secret_nonce,secret_ciphertext FROM integrations")
        .fetch_one(source.reader())
        .await
        .unwrap();
    source.close().await;
    fs::copy(fixture.join("encryption.key"), state.join("encryption.key")).unwrap();
    sqlx::query("INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES(?,'mangadex','Owned','https://example.invalid',1,'{}',1,?,?)")
        .bind(secret.get::<String,_>("id")).bind(secret.get::<Vec<u8>,_>("secret_nonce")).bind(secret.get::<Vec<u8>,_>("secret_ciphertext")).execute(&mut db).await.unwrap();
    sqlx::raw_sql("INSERT INTO publications(id,content_type,title,sort_title) VALUES('p','manga','Owned','Owned');
        INSERT INTO editions(id,publication_id,language) VALUES('e','p','en');
        INSERT INTO units(id,edition_id,label,kind) VALUES('u','e','1','chapter');
        INSERT INTO library_roots(id,label,path) VALUES('r','Owned','/owned-fixture');
        INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path,link_digest,source_kind) SELECT 's','owner',id,'fingerprint','','digest','mangadex_chapter' FROM integrations;
        INSERT INTO direct_chapter_selections VALUES('s','chapter','manga','en',printf('%064d',0),2);
        INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,attempted,manifest_identity,expected_pages) VALUES('a','owner','key','request','s','u','r','r','owned.cbz','journal','needs_review','interrupted_transfer',1,printf('%064d',0),2);")
        .execute(&mut db).await.unwrap();
    let before: String=sqlx::query_scalar("SELECT json_array(attempted,manifest_identity,expected_pages,state,reason,idempotency_key,request_fingerprint) FROM direct_acquisitions").fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    let saved = root.path().join("backup");
    backup::backup_existing(&state, &saved).await.unwrap();
    let snapshot = saved.join("snapshot/library.sqlite3");
    let saved_bytes = fs::read(&snapshot).unwrap();
    let upgraded = SqliteStore::open(&state).await.unwrap();
    let after: String=sqlx::query_scalar("SELECT json_array(attempted,manifest_identity,expected_pages,state,reason,idempotency_key,request_fingerprint) FROM direct_acquisitions").fetch_one(upgraded.reader()).await.unwrap();
    assert_eq!(before, after);
    let mut tx = upgraded.begin_write().await.unwrap();
    assert!(
        sqlx::query(
            "UPDATE direct_acquisitions SET manifest_identity=printf('%064d',1) WHERE id='a'"
        )
        .execute(&mut *tx)
        .await
        .is_err()
    );
    sqlx::query("UPDATE direct_acquisitions SET reason='transfer_restart_required' WHERE id='a'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM direct_review_revisions WHERE acquisition_id='a'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(revision, 1);
    sqlx::query(
        "UPDATE direct_acquisitions SET state='canceled',canceled_at=unixepoch() WHERE id='a'",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE direct_acquisitions SET attempted=0 WHERE id='a'")
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(upgraded.reader())
            .await
            .unwrap()
            .is_empty()
    );
    upgraded.close().await;
    // Rollback preserves the pre-upgrade snapshot; only fresh restore migrates forward.
    let mut rollback = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&snapshot)
            .read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&mut rollback)
        .await
        .unwrap();
    assert_eq!(version, 23);
    let retained:String=sqlx::query_scalar("SELECT json_array(attempted,manifest_identity,expected_pages,state,reason,idempotency_key,request_fingerprint) FROM direct_acquisitions").fetch_one(&mut rollback).await.unwrap();
    assert_eq!(before, retained);
    rollback.close().await.unwrap();
    backup::restore(&saved, &root.path().join("restored"))
        .await
        .unwrap();
    assert_eq!(fs::read(snapshot).unwrap(), saved_bytes);
}
