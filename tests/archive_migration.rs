use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn extending_provider_kinds_preserves_references_and_encrypted_bytes() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut connection)
        .await
        .unwrap();
    for migration in [
        include_str!("../migrations/000_service.sql"),
        include_str!("../migrations/001_catalog.sql"),
        include_str!("../migrations/002_auth.sql"),
        include_str!("../migrations/003_files.sql"),
        include_str!("../migrations/004_jobs.sql"),
        include_str!("../migrations/005_reader.sql"),
        include_str!("../migrations/006_imports.sql"),
        include_str!("../migrations/007_settings.sql"),
        include_str!("../migrations/008_acquisition.sql"),
        include_str!("../migrations/009_monitoring.sql"),
        include_str!("../migrations/010_direct.sql"),
        include_str!("../migrations/011_mangadex_direct.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO integrations VALUES ('source','mangadex','Source','https://example.invalid/',1,'{}',1,zeroblob(24),zeroblob(28))").execute(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path) VALUES ('selection','owner','source','fingerprint','/post')").execute(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO search_releases VALUES ('release','owner','source','fingerprint',1,'digest','magazine','usenet','query',0,20,100)").execute(&mut connection).await.unwrap();
    let before: (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT secret_nonce, secret_ciphertext FROM integrations WHERE id='source'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    let mut tx = connection.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/012_archive_source.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let after: (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT secret_nonce, secret_ciphertext FROM integrations WHERE id='source'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM direct_selections WHERE integration_id='source'"
        )
        .fetch_one(&mut connection)
        .await
        .unwrap(),
        1
    );
    assert!(
        sqlx::query("DELETE FROM integrations WHERE id='source'")
            .execute(&mut connection)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO integrations SELECT 'archive','internetarchive','Archive','https://archive.org/',1,options,secret_version,secret_nonce,secret_ciphertext FROM integrations WHERE id='source'").execute(&mut connection).await.unwrap();
    assert!(
        sqlx::query("UPDATE integrations SET kind='unknown' WHERE id='archive'")
            .execute(&mut connection)
            .await
            .is_err()
    );
    connection.close().await.unwrap();
}
