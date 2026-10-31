use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn move_upgrade_preserves_journals_reservations_and_foreign_keys() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut connection)
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations.iter().filter(|migration| migration.version < 20) {
        sqlx::raw_sql(migration.sql.clone())
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p','comic','Owned','Owned');
         INSERT INTO editions(id,publication_id,language) VALUES('e','p','en');
         INSERT INTO units(id,edition_id,label,kind) VALUES('u','e','1','issue');
         INSERT INTO library_roots(id,label,path) VALUES('r','Owned','/owned-fixture');
         INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('f','/owned-fixture/file.pdf','pdf','signature',1);",
    ).execute(&mut connection).await.unwrap();
    for (index, phase) in [
        "planned",
        "staged",
        "verified",
        "finalized",
        "cataloged",
        "cleanup_pending",
        "done",
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO import_operations(id,source_root,destination_root,destination_relative,destination_path,unit_id,request_json,identity_json,phase,effective_policy,staged_identity_json,validation,library_file_id,last_error,created_at,updated_at) VALUES(?,'r','r',?,?,'u','legacy request','legacy identity',?,?,'staged identity','validation','f','retry reason',10,20)")
            .bind(index.to_string()).bind(format!("{index}.pdf")).bind(format!("/owned-fixture/{index}.pdf"))
            .bind(phase).bind(if index % 2 == 0 { "copy" } else { "hardlink" }).execute(&mut connection).await.unwrap();
    }
    let snapshot = "SELECT json_group_array(json_object('id',id,'source_root',source_root,'destination_root',destination_root,'destination_relative',destination_relative,'destination_path',destination_path,'unit_id',unit_id,'request_json',request_json,'identity_json',identity_json,'phase',phase,'effective_policy',effective_policy,'staged_identity_json',staged_identity_json,'validation',validation,'library_file_id',library_file_id,'last_error',last_error,'created_at',created_at,'updated_at',updated_at)) FROM (SELECT * FROM import_operations ORDER BY id)";
    let before: String = sqlx::query_scalar(snapshot)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/020_import_move.sql"))
        .execute(&mut connection)
        .await
        .unwrap();
    let after: String = sqlx::query_scalar(snapshot)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM import_operations WHERE source_cleanup_state='pending'"
        )
        .fetch_one(&mut connection)
        .await
        .unwrap(),
        7
    );
    sqlx::query("UPDATE import_operations SET effective_policy='move' WHERE id='0'")
        .execute(&mut connection)
        .await
        .unwrap();
    for invalid in [
        "UPDATE import_operations SET effective_policy='unknown' WHERE id='0'",
        "UPDATE import_operations SET phase='unknown' WHERE id='0'",
        "UPDATE import_operations SET source_cleanup_state='unknown' WHERE id='0'",
        "UPDATE import_operations SET destination_path='/owned-fixture/1.pdf' WHERE id='0'",
        "UPDATE import_operations SET destination_relative='1.pdf' WHERE id='0'",
        "DELETE FROM library_roots WHERE id='r'",
        "DELETE FROM units WHERE id='u'",
        "DELETE FROM library_files WHERE id='f'",
    ] {
        assert!(
            sqlx::query(invalid).execute(&mut connection).await.is_err(),
            "{invalid}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        0
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='import_operations_phase'").fetch_one(&mut connection).await.unwrap(), 1);
}
