use std::os::unix::fs::MetadataExt;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use libraryd::{
    app,
    auth::{AuthService, Scope},
    store::sqlite::SqliteStore,
};

struct Fixture {
    directory: tempfile::TempDir,
    store: SqliteStore,
    app: Router,
    read: String,
    manage: String,
    user_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let store = SqliteStore::open(&directory.path().join("state"))
            .await
            .unwrap();
        let auth = AuthService::new(store.clone());
        let login = auth
            .setup("owner", "correct horse battery staple")
            .await
            .unwrap();
        let read = auth.create_key("reader", Scope::Read).await.unwrap().secret;
        let manage = auth
            .create_key("manager", Scope::Manage)
            .await
            .unwrap()
            .secret;
        Self {
            app: app::router(store.clone()),
            directory,
            store,
            read,
            manage,
            user_id: login.principal.user_id,
        }
    }

    async fn exec(&self, statements: &[&'static str]) {
        let mut tx = self.store.begin_write().await.unwrap();
        for statement in statements {
            let mut query = sqlx::query(*statement);
            if statement.contains("?1") {
                query = query.bind(&self.user_id);
            }
            query
                .execute(&mut *tx)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        tx.commit().await.unwrap();
    }

    async fn get(&self, path: &str, token: Option<&str>) -> axum::response::Response {
        let mut request = Request::builder().method("GET").uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn json(&self, path: &str, token: &str) -> Value {
        let response = self.get(path, Some(token)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
}

// Statements bind the caller's user id to `?1` where they reference it.
const CATALOG: &[&str] = &[
    "INSERT INTO publications(id,content_type,title,sort_title) VALUES('empty','comic','Empty','A Empty')",
    "INSERT INTO publications(id,content_type,title,sort_title) VALUES('pub','manga','Two editions','B Two')",
    "INSERT INTO editions(id,publication_id,language) VALUES('ed-fr','pub','fr')",
    "INSERT INTO editions(id,publication_id,language) VALUES('ed-en','pub','en')",
    "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('fr-1','ed-fr','1','volume','1')",
    "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('en-1','ed-en','1','volume','1')",
    "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('en-2','ed-en','2','volume','2')",
    "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES('en-3','ed-en','3','volume','3')",
    "INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('file-a','/x/a.cbz','cbz','a',1)",
    "INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('file-c','/x/c.cbz','cbz','c',1)",
    "INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('file-a','fr-1','user_confirmed')",
    "INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('file-c','en-3','user_confirmed')",
    "INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('file-c','en-2','user_confirmed')",
    "INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES('prowlarr','prowlarr','P','http://127.0.0.1:1',1,'{}',1,zeroblob(24),zeroblob(28))",
    "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m-own',?1,'en-1','prowlarr','manga','q',900,'review_only',1,0,'scheduled')",
    "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m-disabled',?1,'en-2','prowlarr','manga','q',900,'review_only',0,0,'disabled')",
    "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state) VALUES('m-other','someone-else','fr-1','prowlarr','manga','q',900,'review_only',1,0,'scheduled')",
];

#[tokio::test]
async fn publication_availability_counts_files_caller_monitors_and_cover() {
    let fixture = Fixture::new().await;
    fixture.exec(CATALOG).await;
    let root = fixture.directory.path().join("library");
    std::fs::create_dir(&root).unwrap();
    let target = root.join("b.cbz");
    std::fs::copy("tests/fixtures/natural-order.cbz", &target).unwrap();
    let signature: String = Sha256::digest(std::fs::read(&target).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let metadata = target.metadata().unwrap();
    let mut tx = fixture.store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO library_roots(id,label,path) VALUES('root','Library',?)")
        .bind(root.to_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    // Largest file id, but English and unit order put it first, so it is the cover.
    sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path,mtime_ns) VALUES('file-z',?,'cbz',?,?,'root','b.cbz',?)")
        .bind(target.to_str().unwrap()).bind(&signature).bind(metadata.len() as i64)
        .bind(metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec())
        .execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('file-z','en-1','user_confirmed')")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    let page = fixture.json("/api/v1/publications", &fixture.read).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "empty");
    assert_eq!(items[0]["file_count"], 0);
    assert_eq!(items[0]["monitored_unit_count"], 0);
    assert_eq!(items[0]["cover_file_id"], Value::Null);
    assert_eq!(items[0]["title"], "Empty");
    assert_eq!(items[1]["id"], "pub");
    assert_eq!(items[1]["file_count"], 3);
    assert_eq!(items[1]["monitored_unit_count"], 1);
    assert_eq!(items[1]["cover_file_id"], "file-z");

    let detail = fixture
        .json("/api/v1/publications/pub", &fixture.read)
        .await;
    assert_eq!(detail, items[1]);
    let detail = fixture
        .json("/api/v1/publications/empty", &fixture.read)
        .await;
    assert_eq!(detail, items[0]);

    let thumbnail = fixture
        .get(
            "/api/v1/library/files/file-z/thumbnail",
            Some(&fixture.read),
        )
        .await;
    assert_eq!(thumbnail.status(), StatusCode::OK);
    assert_eq!(
        thumbnail.headers()[header::CACHE_CONTROL],
        "private, max-age=86400"
    );
    assert_eq!(thumbnail.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        thumbnail.headers()[header::ETAG],
        format!("\"{signature}\"").as_str()
    );
    assert_eq!(
        fixture
            .get("/api/v1/library/files/file-z/thumbnail", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

const REVIEW: &[&str] = &[
    "INSERT INTO library_roots(id,label,path) VALUES('root','Library','/srv/library')",
    "INSERT INTO scan_runs(id,root_id,state,started_at) VALUES('run','root','completed',1000)",
    "INSERT INTO scan_entries(id,root_id,relative_path,format,signature,size_bytes,mtime_ns,state,last_seen_run_id) VALUES('entry-new','root','new.cbz','cbz','n',1,1,'pending_association','run')",
    "INSERT INTO scan_entries(id,root_id,relative_path,format,signature,size_bytes,mtime_ns,state,last_seen_run_id) VALUES('entry-linked','root','linked.cbz','cbz','l',1,1,'pending_association','run')",
    "INSERT INTO library_files(id,path,format,signature,size_bytes) VALUES('linked','/srv/library/linked.cbz','cbz','l',1)",
    "INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES('linked','en-1','user_confirmed')",
    "INSERT INTO jobs(id,kind,scope,payload,payload_fingerprint,state,reason,created_at,updated_at) VALUES('job-scan','library.scan','root:root','{}','f1','failed','io',2000,2000)",
    "INSERT INTO jobs(id,kind,scope,payload,payload_fingerprint,state,created_at,updated_at) VALUES('job-done','library.scan','root:root','{}','f2','completed',2001,2001)",
    "INSERT INTO jobs(id,kind,scope,payload,payload_fingerprint,state,reason,created_at,updated_at) VALUES('job-own','acquisition','a','{}','f3','needs_review','uncertain_submission',3000,3000)",
    "INSERT INTO jobs(id,kind,scope,payload,payload_fingerprint,state,reason,created_at,updated_at) VALUES('job-other','acquisition','b','{}','f4','needs_review','uncertain_submission',3001,3001)",
    "INSERT INTO search_releases(handle,owner,integration_id,source_fingerprint,indexer_id,guid_digest,content_type,protocol,query,search_offset,search_limit,expires_at) VALUES('rel-own',?1,'prowlarr','s',1,'g1','manga','torrent','q',0,10,9999999999)",
    "INSERT INTO search_releases(handle,owner,integration_id,source_fingerprint,indexer_id,guid_digest,content_type,protocol,query,search_offset,search_limit,expires_at) VALUES('rel-other','someone-else','prowlarr','s',1,'g2','manga','torrent','q',0,10,9999999999)",
    "INSERT INTO acquisition_intents(id,caller,idempotency_key,request,request_fingerprint,job_id,created_at) VALUES('acq-own',?1,'k1','{}','r1','job-own',3000)",
    "INSERT INTO acquisition_intents(id,caller,idempotency_key,request,request_fingerprint,job_id,created_at) VALUES('acq-other','someone-else','k2','{}','r2','job-other',3001)",
    "INSERT INTO acquisition_runs(id,release_handle,client_id,client_fingerprint,client_kind,category,unit_id,state,reason,created_at) VALUES('acq-own','rel-own','prowlarr','c','qbittorrent','x','en-2','needs_review','uncertain_submission',3000)",
    "INSERT INTO acquisition_runs(id,release_handle,client_id,client_fingerprint,client_kind,category,unit_id,state,reason,created_at) VALUES('acq-other','rel-other','prowlarr','c','qbittorrent','x','en-3','needs_review','uncertain_submission',3001)",
    "INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path) VALUES('sel-own',?1,'prowlarr','s','/p1')",
    "INSERT INTO direct_selections(handle,owner,integration_id,source_fingerprint,post_path) VALUES('sel-other','someone-else','prowlarr','s','/p2')",
    "INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,created_at) VALUES('direct-own',?1,'k','r','sel-own','fr-1','root','root','d1','i1','needs_review','verification_failed',4000)",
    "INSERT INTO direct_acquisitions(id,owner,idempotency_key,request_fingerprint,link_handle,unit_id,download_root_id,destination_root_id,destination_relative,import_id,state,reason,created_at) VALUES('direct-other','someone-else','k','r','sel-other','en-1','root','root','d2','i2','needs_review','verification_failed',4001)",
    "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state,last_run_at) VALUES('m-review',?1,'en-3','prowlarr','manga','q',900,'review_only',1,0,'needs_review',5000)",
    "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state,last_run_at) VALUES('m-review-other','someone-else','en-3','prowlarr','manga','q',900,'review_only',1,0,'needs_review',5001)",
];

#[tokio::test]
async fn empty_review_feed_requires_authentication() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture.get("/api/v1/review", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let feed = fixture.json("/api/v1/review", &fixture.read).await;
    assert_eq!(
        feed,
        serde_json::json!({"items": [], "totals": {
            "acquisition_needs_review": 0, "direct_acquisition_needs_review": null,
            "file_to_link": null, "job_failed": 0, "monitor_needs_review": 0}})
    );
    let feed = fixture.json("/api/v1/review", &fixture.manage).await;
    assert_eq!(feed["totals"]["file_to_link"], 0);
    assert_eq!(feed["totals"]["direct_acquisition_needs_review"], 0);
}

#[tokio::test]
async fn review_feed_keeps_each_source_access_rule() {
    let fixture = Fixture::new().await;
    fixture.exec(CATALOG).await;
    fixture.exec(REVIEW).await;
    let response = fixture.get("/api/v1/review", Some(&fixture.manage)).await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let feed = fixture.json("/api/v1/review", &fixture.manage).await;
    assert_eq!(
        feed["totals"],
        serde_json::json!({"acquisition_needs_review": 1, "direct_acquisition_needs_review": 1,
            "file_to_link": 1, "job_failed": 2, "monitor_needs_review": 1})
    );
    let summary: Vec<(String, String)> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap().to_owned(),
                item["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("monitor_needs_review", "m-review"),
            ("direct_acquisition_needs_review", "direct-own"),
            ("job_failed", "job-other"),
            ("acquisition_needs_review", "acq-own"),
            ("job_failed", "job-scan"),
            ("file_to_link", "entry-new"),
        ]
        .map(|(kind, id)| (kind.to_owned(), id.to_owned()))
    );
    let items = feed["items"].as_array().unwrap();
    assert_eq!(
        items[3],
        serde_json::json!({"kind": "acquisition_needs_review", "id": "acq-own", "created_at": 3000,
            "state": "needs_review", "reason": "uncertain_submission", "publication_id": "pub",
            "publication_title": "Two editions", "unit_id": "en-2",
            "unit": {"id":"en-2","edition_id":"ed-en","label":"2","kind":"volume","sort_key":"2","date":null,"date_precision":null},
            "acquisition_id": "acq-own", "job_id": "job-own",
            "job_kind": "acquisition", "root_id": null, "entry_id": null, "relative_path": null,
            "monitor_id": null})
    );
    assert_eq!(
        items[5],
        serde_json::json!({"kind": "file_to_link", "id": "entry-new", "created_at": 1000,
            "state": "pending_association", "reason": null, "publication_id": null,
            "publication_title": null, "unit_id": null, "unit": null,
            "acquisition_id": null, "job_id": null, "job_kind": null,
            "root_id": "root", "entry_id": "entry-new", "relative_path": "new.cbz",
            "monitor_id": null})
    );
    assert_eq!(items[0]["monitor_id"], "m-review");
    assert_eq!(items[0]["publication_id"], "pub");
    assert_eq!(items[1]["acquisition_id"], "direct-own");

    let feed = fixture.json("/api/v1/review", &fixture.read).await;
    assert_eq!(feed["totals"]["file_to_link"], Value::Null);
    assert_eq!(
        feed["totals"]["direct_acquisition_needs_review"],
        Value::Null
    );
    assert_eq!(feed["totals"]["acquisition_needs_review"], 1);
    assert!(feed["items"].as_array().unwrap().iter().all(|item| {
        item["kind"] != "file_to_link" && item["kind"] != "direct_acquisition_needs_review"
    }));
}

#[tokio::test]
async fn review_catalog_identities_include_unit_kind_and_date_precision() {
    let fixture = Fixture::new().await;
    fixture.exec(CATALOG).await;
    fixture.exec(REVIEW).await;
    fixture.exec(&[
        "UPDATE units SET label='May 2026', kind='issue', date='2026-05', date_precision='month' WHERE id='en-2'",
        "UPDATE units SET label='8', kind='chapter', date='2026', date_precision='year' WHERE id='fr-1'",
        "UPDATE units SET label='12', kind='volume', date='2026-05-14', date_precision='day' WHERE id='en-3'",
    ]).await;
    let feed = fixture.json("/api/v1/review", &fixture.manage).await;
    for (kind, unit_id, label, unit_kind, date, precision) in [
        (
            "acquisition_needs_review",
            "en-2",
            "May 2026",
            "issue",
            "2026-05",
            "month",
        ),
        (
            "direct_acquisition_needs_review",
            "fr-1",
            "8",
            "chapter",
            "2026",
            "year",
        ),
        (
            "monitor_needs_review",
            "en-3",
            "12",
            "volume",
            "2026-05-14",
            "day",
        ),
    ] {
        let item = feed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == kind)
            .unwrap();
        assert_eq!(item["publication_title"], "Two editions");
        assert_eq!(item["unit_id"], unit_id);
        assert_eq!(item["unit"]["id"], unit_id);
        assert_eq!(item["unit"]["label"], label);
        assert_eq!(item["unit"]["kind"], unit_kind);
        assert_eq!(item["unit"]["date"], date);
        assert_eq!(item["unit"]["date_precision"], precision);
    }
    for item in feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "job_failed" || item["kind"] == "file_to_link")
    {
        assert!(item["publication_title"].is_null());
        assert!(item["unit"].is_null());
    }
}
