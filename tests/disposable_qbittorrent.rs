//! Mutating acceptance for scripts/check-qbittorrent.py only. Never reads live-client config.
use libraryd::clients::{
    AuthorizedPayload, ClientConfig, ClientError, ClientKind, DownloadState, HttpLimits, OwnedJob,
    QBittorrent, SubmissionAttempt,
};
use libraryd::{
    importer::journal::{ImportPhase, ImportPolicy, ImportService, InternalImportRequest},
    library::roots::Library,
    store::sqlite::SqliteStore,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

const DOWNLOAD: &str = "http://127.0.0.1:8080/";
const SEED: &str = "http://127.0.0.1:8081/";
const POLL: Duration = Duration::from_millis(500);
const LIMIT: usize = 1024 * 1024;
const CBZ: &[u8] = include_bytes!("fixtures/natural-order.cbz");
const STOPPED_EVENT: &str = "Torrent stopped. Torrent: \"real.cbz\"";
const RESUMED_EVENT: &str = "Torrent resumed. Torrent: \"real.cbz\"";

#[derive(Deserialize)]
struct LogEntry {
    id: i64,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    run_id: Uuid,
    infohash: String,
    sha256: String,
    length: usize,
    seed_password: String,
    download_password: String,
    netns: String,
}

// Raw API is used only for fixture setup, explicit peer injection/recheck, and independent
// ownership/completion observations. All downloader lifecycle writes use the adapter.
struct FixtureApi {
    client: reqwest::Client,
    base: &'static str,
}
impl FixtureApi {
    fn new(base: &'static str) -> Self {
        assert!([DOWNLOAD, SEED].contains(&base));
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .cookie_store(true)
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base,
        }
    }
    fn request(&self, path: &str, post: bool) -> reqwest::RequestBuilder {
        let url = format!("{}api/v2/{path}", self.base);
        let request = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        };
        request
            .header("Referer", self.base)
            .header("Origin", self.base.trim_end_matches('/'))
    }
    async fn body(request: reqwest::RequestBuilder) -> Vec<u8> {
        let mut response = request.send().await.expect("fixture HTTP request");
        assert!(
            response.status().is_success(),
            "fixture API rejected operation"
        );
        assert!(response.content_length().is_none_or(|n| n <= LIMIT as u64));
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.expect("fixture response body") {
            assert!(chunk.len() <= LIMIT - bytes.len());
            bytes.extend_from_slice(&chunk);
        }
        bytes
    }
    async fn login(&self, password: &str) {
        let body = Self::body(
            self.request("auth/login", true)
                .form(&[("username", "acceptance"), ("password", password)]),
        )
        .await;
        assert!(
            body.is_empty() || body == b"Ok.",
            "fresh fixture authentication failed"
        );
    }
    async fn get(&self, path: &str) -> Value {
        serde_json::from_slice(&Self::body(self.request(path, false)).await).unwrap()
    }
    async fn post(&self, path: &str, form: &[(&str, &str)]) {
        Self::body(self.request(path, true).form(form)).await;
    }
    async fn logs_since(&self, watermark: i64) -> Vec<LogEntry> {
        let bytes = Self::body(self.request("log/main", false).query(&[
            ("normal", "true"),
            ("info", "false"),
            ("warning", "false"),
            ("critical", "false"),
            ("last_known_id", &watermark.to_string()),
        ]))
        .await;
        let rows: Vec<LogEntry> = serde_json::from_slice(&bytes).expect("fixture log schema");
        assert!(rows.len() <= 256, "fixture log event cap");
        let mut previous = watermark;
        for row in &rows {
            assert!(row.id > previous, "fixture log IDs must advance");
            previous = row.id;
        }
        rows
    }
    async fn wait_log_events(&self, mut watermark: i64, events: &[&str]) -> i64 {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut next = 0;
            loop {
                for row in self.logs_since(watermark).await {
                    watermark = row.id;
                    if row.message == events[next] {
                        next += 1;
                        if next == events.len() {
                            return watermark;
                        }
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("post-request owned torrent events did not arrive within 30 seconds")
    }
    async fn torrent(&self, hash: &str) -> Option<Value> {
        let body = Self::body(
            self.request("torrents/info", false)
                .query(&[("hashes", hash)]),
        )
        .await;
        let mut rows: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert!(rows.len() <= 1);
        rows.pop()
    }
    async fn completed_transfer(&self, hash: &str, length: usize) -> Value {
        // 5.2.3 downloaded uses all_time_download, accumulated on libtorrent's
        // statistics tick. Completion can precede that tick for this tiny fixture.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let row = self
                    .torrent(hash)
                    .await
                    .expect("owned transfer remains registered");
                let downloaded = row["downloaded"].as_u64().expect("all-time byte counter");
                let session = row["downloaded_session"]
                    .as_u64()
                    .expect("session payload byte counter");
                assert!(!matches!(
                    row["state"].as_str(),
                    Some("error" | "missingFiles")
                ));
                if row["progress"].as_f64() == Some(1.0)
                    && matches!(
                        row["state"].as_str(),
                        Some("uploading" | "stalledUP" | "forcedUP" | "stoppedUP")
                    )
                    && downloaded >= length as u64
                    && downloaded == session
                {
                    return row;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("completed transfer counters did not converge within 15 seconds")
    }
    async fn peer_count(&self, hash: &str) -> usize {
        // rid=0 requests a full snapshot; the handler fetches live peer info.
        let bytes = Self::body(
            self.request("sync/torrentPeers", false)
                .query(&[("hash", hash), ("rid", "0")]),
        )
        .await;
        let snapshot: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot["full_update"], true, "full peer snapshot required");
        snapshot["peers"]
            .as_object()
            .expect("peer snapshot map")
            .len()
    }
    async fn configure(&self, port: u16) {
        let prefs = json!({"dht":false,"pex":false,"lsd":false,"upnp":false,
            "listen_port":port,"random_port":false,"save_path":"/downloads/",
            "temp_path_enabled":false,"auto_tmm_enabled":false,"queueing_enabled":false,
            "autorun_enabled":false,"autorun_on_torrent_added_enabled":false,
            "rss_processing_enabled":false,"rss_auto_downloading_enabled":false});
        self.post("app/setPreferences", &[("json", &prefs.to_string())])
            .await;
        let effective = self.get("app/preferences").await;
        for key in [
            "dht",
            "pex",
            "lsd",
            "upnp",
            "temp_path_enabled",
            "auto_tmm_enabled",
        ] {
            assert_eq!(effective[key], false, "fixture preference {key}");
        }
        assert_eq!(effective["listen_port"], port);
        assert_eq!(
            effective["save_path"]
                .as_str()
                .unwrap()
                .trim_end_matches('/'),
            "/downloads"
        );
        assert_eq!(
            self.get("torrents/info").await,
            json!([]),
            "fixture queue must start empty"
        );
    }
}

async fn seed_isolation(seed: &FixtureApi, download: &FixtureApi, hash: &str) -> (bool, usize) {
    let row = seed
        .torrent(hash)
        .await
        .expect("owned seed remains registered");
    assert_eq!(row["hash"].as_str(), Some(hash));
    (
        row["state"].as_str() == Some("stoppedUP"),
        download.peer_count(hash).await,
    )
}

fn adapter(base: &str, password: &str, id: Uuid, category: &str) -> QBittorrent {
    QBittorrent::new(
        ClientConfig::new(
            id,
            base,
            category.to_owned(),
            HttpLimits {
                timeout: Duration::from_secs(5),
                max_response_bytes: LIMIT,
            },
        )
        .unwrap(),
        "acceptance".into(),
        password.into(),
    )
    .unwrap()
}

async fn ready(client: &QBittorrent) -> String {
    // Connection readiness is read-only. Never retry enqueue or lifecycle writes.
    loop {
        match client.test_connection().await {
            Ok(info) => return info.version,
            Err(ClientError::Unavailable) => tokio::time::sleep(POLL).await,
            Err(error) => panic!("fresh disposable client authentication/version failed: {error}"),
        }
    }
}

async fn wait_state(client: &QBittorrent, job: &OwnedJob, wanted: &[DownloadState]) {
    loop {
        let status = client.status(job).await.expect("owned adapter status");
        assert_ne!(status.state, DownloadState::Failed, "owned torrent failed");
        if wanted.contains(&status.state) {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

fn write_evidence(root: &Path, filename: &str, value: &Value) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(filename))
        .expect("new disposable evidence file");
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
}

fn assert_payload(root: &Path, config: &Config) {
    let path = root.join("download/downloads/real.cbz");
    let metadata = std::fs::symlink_metadata(&path).expect("completed owned payload exists");
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), config.length as u64);
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        config.sha256
    );
    assert_eq!(bytes, CBZ);
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(metadata.is_file());
    (metadata.dev(), metadata.ino())
}

async fn import_seeding_payload(root: &Path) {
    for name in ["import-state", "library"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join(name))
            .unwrap();
    }
    let store = SqliteStore::open(&root.join("import-state")).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("INSERT INTO publications(id,content_type,title,sort_title) VALUES('publication','comic','Owned CBZ fixture','Owned CBZ fixture')").execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO editions(id,publication_id,language) VALUES('edition','publication','en')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO units(id,edition_id,label,kind) VALUES('unit','edition','1','issue')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let library = Library::new(store.clone());
    let source_root = library
        .register_root("download", &root.join("download/downloads"))
        .await
        .unwrap()
        .id;
    let destination_root = library
        .register_root("library", &root.join("library"))
        .await
        .unwrap()
        .id;
    let source = root.join("download/downloads/real.cbz");
    let original = identity(&source);
    let service = ImportService::new(store.clone());
    for (name, policy, effective) in [
        (
            "hardlink.cbz",
            ImportPolicy::Hardlink {
                fallback_to_copy: false,
            },
            "hardlink",
        ),
        ("copy.cbz", ImportPolicy::Copy, "copy"),
    ] {
        let operation_id = Uuid::new_v4().to_string();
        write_evidence(
            root,
            &format!("{effective}-import-intent.json"),
            &json!({"operation_id":operation_id}),
        );
        service
            .plan(
                &operation_id,
                InternalImportRequest {
                    source_root: source_root.clone(),
                    source_relative: "real.cbz".into(),
                    destination_root: destination_root.clone(),
                    destination_relative: name.into(),
                    unit_id: "unit".into(),
                    policy,
                },
            )
            .await
            .unwrap();
        let result = service.recover(&operation_id).await.unwrap();
        assert_eq!(result.phase, ImportPhase::Done);
        assert_eq!(result.effective_policy.as_deref(), Some(effective));
        assert!(result.library_file_id.is_some());
        assert_eq!(std::fs::read(&source).unwrap(), CBZ);
        assert_eq!(identity(&source), original);
        let destination = root.join("library").join(name);
        assert_eq!(std::fs::read(&destination).unwrap(), CBZ);
        assert_eq!(identity(&destination) == original, effective == "hardlink");
        write_evidence(
            root,
            &format!("{effective}-import-result.json"),
            &serde_json::to_value(result).unwrap(),
        );
    }
    drop(service);
    drop(library);
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mutates disposable queues; use scripts/check-qbittorrent.py with explicit opt-in"]
async fn disposable_owned_qbittorrent() {
    // Only static adapter stages and redacted ClientError variants are emitted.
    tracing_subscriber::fmt()
        .with_env_filter("off,libraryd::clients::qbittorrent=warn")
        .with_ansi(false)
        .without_time()
        .try_init()
        .expect("owned acceptance tracing subscriber");
    assert_eq!(
        std::env::var("LIBRARY_DISPOSABLE_QBITT").as_deref(),
        Ok("I_ACCEPT_OWNED_QUEUE_WRITES")
    );
    let config_path =
        std::env::var_os("LIBRARY_DISPOSABLE_QBITT_CONFIG").expect("harness-owned config required");
    let path = Path::new(&config_path);
    let root = path.parent().unwrap();
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(metadata.is_file() && metadata.len() < 8192);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, 0o700);
    let config: Config = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        std::fs::read_link("/proc/self/ns/net").unwrap().to_str(),
        Some(config.netns.as_str())
    );
    // The harness verifies Docker's seed=none and helper/download=container:seed
    // relationships. PID 1 is private to this helper and shares our network namespace.
    assert_eq!(unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }, 8);
    for line in std::fs::read_to_string("/proc/net/dev")
        .unwrap()
        .lines()
        .skip(2)
    {
        assert_eq!(
            line.split(':').next().unwrap().trim(),
            "lo",
            "only disposable loopback is permitted"
        );
    }
    assert_eq!(config.length, CBZ.len());
    assert!(config.length > 0 && config.length <= LIMIT);
    assert!(!root.join("download/downloads/real.cbz").exists());
    let torrent = std::fs::read(root.join("owned.torrent")).unwrap();
    assert_eq!(
        libraryd::search::torrent::v1_infohash(&torrent).unwrap(),
        config.infohash
    );
    let scenario = async {
        let category = format!("acceptance-{}", config.run_id.simple());
        let client_id = Uuid::new_v4();
        let own_id = Uuid::new_v4();
        let download = adapter(DOWNLOAD, &config.download_password, client_id, &category);
        let seed_adapter = adapter(SEED, &config.seed_password, Uuid::new_v4(), "fixture-seed");
        let download_version = ready(&download).await;
        let seed_version = ready(&seed_adapter).await;
        assert_eq!(download_version, "5.2.3");
        assert_eq!(seed_version, "5.2.3");
        let seed = FixtureApi::new(SEED);
        let raw = FixtureApi::new(DOWNLOAD);
        seed.login(&config.seed_password).await;
        raw.login(&config.download_password).await;
        seed.configure(6882).await;
        raw.configure(6881).await;
        raw.post(
            "torrents/createCategory",
            &[("category", &category), ("savePath", "/downloads/")],
        )
        .await;
        let seed_receipt = FixtureApi::body(
            seed.request("torrents/add", true).multipart(
                reqwest::multipart::Form::new()
                    .text("savepath", "/downloads/")
                    .text("contentLayout", "Original")
                    .text("tags", "fixture-seed")
                    .part(
                        "torrents",
                        reqwest::multipart::Part::bytes(torrent.clone()).file_name("owned.torrent"),
                    ),
            ),
        )
        .await;
        // The pinned 5.2.3 API returns JSON, including for uploaded torrent files.
        let seed_receipt: Value =
            serde_json::from_slice(&seed_receipt).expect("qBittorrent 5.2.3 JSON add receipt");
        assert_eq!(seed_receipt["success_count"], 1);
        assert_eq!(seed_receipt["failure_count"], 0);
        assert_eq!(seed_receipt["pending_count"], 0);
        assert_eq!(seed_receipt["added_torrent_ids"], json!([config.infohash]));
        write_evidence(
            root,
            "seed-add-receipt.json",
            &json!({"success_count":1,"failure_count":0,"pending_count":0,
                "added_torrent_ids":[config.infohash]}),
        );
        loop {
            if seed.torrent(&config.infohash).await.is_some_and(|row| {
                row["progress"].as_f64() == Some(1.0)
                    && matches!(
                        row["state"].as_str(),
                        Some("uploading" | "stalledUP" | "forcedUP")
                    )
            }) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        write_evidence(
            root,
            "attempted-fence.json",
            &json!({"run_id":config.run_id,
            "client_id":client_id,"own_id":own_id,"infohash":config.infohash,"attempted":true}),
        );
        let mut attempt = SubmissionAttempt::from_persisted(own_id, false).unwrap();
        let payload =
            || AuthorizedPayload::torrent(torrent.clone(), config.infohash.clone()).unwrap();
        let jobs = download
            .enqueue(&mut attempt, payload())
            .await
            .expect("single real adapter enqueue; no retry");
        assert!(attempt.was_attempted());
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.external_id(), config.infohash);
        assert_eq!(job.own_id(), own_id);
        write_evidence(root, "receipt.json", &serde_json::to_value(job).unwrap());
        let row = raw.torrent(&config.infohash).await.unwrap();
        assert_eq!(row["category"], category);
        let tag = format!("libraryd-{own_id}");
        assert!(
            row["tags"]
                .as_str()
                .unwrap()
                .split(',')
                .any(|s| s.trim() == tag)
        );
        let foreign = OwnedJob::from_persisted_receipt(
            Uuid::new_v4(),
            client_id,
            ClientKind::QBittorrent,
            category.clone(),
            config.infohash.clone(),
        )
        .unwrap();
        assert_eq!(
            download.status(&foreign).await.unwrap_err(),
            ClientError::NotOwned
        );
        download.pause(job).await.expect("adapter pause");
        wait_state(&download, job, &[DownloadState::Paused]).await;
        download.resume(job).await.expect("adapter resume");
        wait_state(
            &download,
            job,
            &[DownloadState::Downloading, DownloadState::Queued],
        )
        .await;
        raw.post(
            "torrents/addPeers",
            &[("hashes", &config.infohash), ("peers", "127.0.0.1:6882")],
        )
        .await;
        wait_state(
            &download,
            job,
            &[DownloadState::Seeding, DownloadState::Completed],
        )
        .await;
        assert_payload(root, &config);
        let completed = raw
            .completed_transfer(&config.infohash, config.length)
            .await;
        assert_eq!(completed["progress"].as_f64(), Some(1.0));
        assert!(completed["downloaded"].as_u64().unwrap() >= config.length as u64);
        write_evidence(
            root,
            "transfer-counters.json",
            &json!({
                "expected_bytes":config.length,"downloaded":completed["downloaded"],
                "downloaded_session":completed["downloaded_session"],
                "exact_payload_verified":true,
            }),
        );
        assert_payload(root, &config);
        // This is the only seed in a trackerless private, loopback-only swarm.
        // Stop it once and drain the downloader's actual connections before import.
        seed.post("torrents/stop", &[("hashes", &config.infohash)])
            .await;
        let mut last_isolation = (false, usize::MAX);
        let isolated = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                last_isolation = seed_isolation(&seed, &raw, &config.infohash).await;
                if last_isolation == (true, 0) {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await;
        assert!(
            isolated.is_ok(),
            "seed isolation deadline: stopped={}, downloader_peers={}",
            last_isolation.0,
            last_isolation.1
        );
        let source = root.join("download/downloads/real.cbz");
        let source_identity = identity(&source);
        import_seeding_payload(root).await;
        assert_eq!(
            seed_isolation(&seed, &raw, &config.infohash).await,
            (true, 0),
            "seed stopped and downloader peers empty after import"
        );
        assert_eq!(identity(&source), source_identity);
        assert_payload(root, &config);
        // Keep the torrent registered throughout import and one explicit recheck.
        download
            .status(job)
            .await
            .expect("owned torrent remains registered after import");
        assert_eq!(
            raw.torrent(&config.infohash).await.unwrap()["name"],
            "real.cbz"
        );
        let watermark = raw
            .logs_since(-1)
            .await
            .last()
            .expect("fixture startup logs")
            .id;
        download
            .pause(job)
            .await
            .expect("stop owned torrent before recheck");
        let stopped_id = raw.wait_log_events(watermark, &[STOPPED_EVENT]).await;
        wait_state(&download, job, &[DownloadState::Completed]).await;
        // 5.2.3 forceRecheck starts a stopped torrent with FilesChecked stop condition.
        // handleTorrentChecked stops it on torrent_checked_alert. The ordered fresh
        // resumed/stopped pair proves completion without observing transient checking.
        // Primary source: release-5.2.3 src/base/bittorrent/torrentimpl.cpp,
        // forceRecheck and handleTorrentChecked; sessionimpl.cpp logs start/stop.
        raw.post("torrents/recheck", &[("hashes", &config.infohash)])
            .await;
        let checked_id = raw
            .wait_log_events(stopped_id, &[RESUMED_EVENT, STOPPED_EVENT])
            .await;
        assert!(checked_id > stopped_id);
        download
            .resume(job)
            .await
            .expect("resume owned torrent after checked alert");
        wait_state(&download, job, &[DownloadState::Seeding]).await;
        let rechecked = raw.torrent(&config.infohash).await.unwrap();
        assert_eq!(rechecked["progress"].as_f64(), Some(1.0));
        let baseline_downloaded = completed["downloaded"].as_u64().unwrap();
        assert_eq!(
            rechecked["downloaded"].as_u64(),
            Some(baseline_downloaded),
            "cumulative transfer counter changed during isolated recheck"
        );
        assert_eq!(
            seed_isolation(&seed, &raw, &config.infohash).await,
            (true, 0),
            "seed stopped and downloader peers empty after recheck"
        );
        write_evidence(
            root,
            "recheck-isolation.json",
            &json!({
                "only_seed_stopped":true,"downloader_peers":0,
                "verified_before_import_after_import_after_recheck":true,
                "downloaded_before":baseline_downloaded,"downloaded_after":rechecked["downloaded"],
                "session_before":completed["downloaded_session"],
                "session_after":rechecked["downloaded_session"],
            }),
        );
        let pieces = FixtureApi::body(
            raw.request("torrents/pieceStates", false)
                .query(&[("hash", &config.infohash)]),
        )
        .await;
        let pieces: Vec<u8> = serde_json::from_slice(&pieces).unwrap();
        assert_eq!(pieces.len(), config.length.div_ceil(16384));
        assert!(pieces.iter().all(|piece| *piece == 2));
        assert_eq!(identity(&source), source_identity);
        assert_payload(root, &config);
        for name in ["hardlink.cbz", "copy.cbz"] {
            assert_eq!(std::fs::read(root.join("library").join(name)).unwrap(), CBZ);
        }
        download
            .remove(job)
            .await
            .expect("adapter removes torrent record only");
        while raw.torrent(&config.infohash).await.is_some() {
            tokio::time::sleep(POLL).await;
        }
        assert_payload(root, &config);
        assert_eq!(identity(&source), source_identity);
        for name in ["hardlink.cbz", "copy.cbz"] {
            assert_eq!(std::fs::read(root.join("library").join(name)).unwrap(), CBZ);
        }
        assert_eq!(
            download.enqueue(&mut attempt, payload()).await.unwrap_err(),
            ClientError::NeedsReview
        );
        let mut restored = SubmissionAttempt::from_persisted(own_id, true).unwrap();
        assert_eq!(
            download
                .enqueue(&mut restored, payload())
                .await
                .unwrap_err(),
            ClientError::NeedsReview
        );
        assert!(
            raw.torrent(&config.infohash).await.is_none(),
            "attempted fence must prevent resubmission"
        );
        write_evidence(
            root,
            "result.json",
            &json!({"passed":true,"run_id":config.run_id,
            "download_version":download_version,"seed_version":seed_version,"infohash":config.infohash,
            "payload_sha256":config.sha256,"payload_bytes":config.length,
            "ownership_checked":true,"pause_resume_checked":true,"payload_preserved":true,
            "hardlink_import_checked":true,"copy_import_checked":true,
            "recheck_completed":true,"source_inode_preserved":true,
            "recheck_baseline_log_id":stopped_id,"recheck_completed_log_id":checked_id,
            "attempted_fence_checked":true}),
        );
    };
    tokio::time::timeout(Duration::from_secs(180), scenario)
        .await
        .expect("180-second disposable acceptance deadline");
}
