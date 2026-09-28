//! Mutating owned-fixture acceptance only; never reads production client settings.
use libraryd::clients::{
    AuthorizedPayload, ClientConfig, ClientError, DownloadState, HttpLimits, OwnedJob, Sabnzbd,
    SubmissionAttempt,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{os::unix::fs::MetadataExt, path::Path, time::Duration};
use uuid::Uuid;

const BASE: &str = "http://127.0.0.1:8080/";
const POLL: Duration = Duration::from_millis(500);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    run_id: Uuid,
    key: String,
    nzb_key: String,
    sha256: String,
    netns: String,
}

fn evidence(name: &str, value: Value) {
    let path = Path::new("/control").join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer(&mut file, &value).unwrap();
    file.sync_all().unwrap();
}
async fn state(client: &Sabnzbd, job: &OwnedJob, expected: DownloadState) {
    loop {
        let status = client.status(job).await.expect("owned status");
        assert_ne!(status.state, DownloadState::Failed, "real transfer failed");
        if status.state == expected {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}
async fn api(http: &reqwest::Client, key: &str, mode: &str, id: Option<&str>) -> Value {
    let mut fields = vec![
        ("apikey", key),
        ("mode", mode),
        ("output", "json"),
        ("limit", "10"),
    ];
    if let Some(id) = id {
        fields.push(("nzo_ids", id));
    }
    let mut response = http
        .post(format!("{BASE}api"))
        .form(&fields)
        .send()
        .await
        .expect("fixture API");
    assert!(response.status().is_success());
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        assert!(bytes.len() + chunk.len() <= 1024 * 1024);
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("error").is_none(), "fixture API rejected request");
    value
}
fn payload(path: &Path, expected: &str) {
    let meta = std::fs::symlink_metadata(path).expect("real completed payload");
    assert!(meta.is_file() && !meta.file_type().is_symlink());
    assert_eq!(meta.len(), 256 * 1024);
    let bytes = std::fs::read(path).unwrap();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, expected);
}
fn find_payload(root: &Path, depth: usize) -> Vec<std::path::PathBuf> {
    assert!(depth <= 5);
    let mut found = Vec::new();
    let entries: Vec<_> = std::fs::read_dir(root).unwrap().collect();
    assert!(entries.len() <= 32);
    for entry in entries {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        if kind.is_dir() {
            found.extend(find_payload(&entry.path(), depth + 1));
        } else if entry.file_name() == "owned-payload.bin" {
            found.push(entry.path());
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/check-sabnzbd.py and explicit owned queue-write opt-in"]
async fn disposable_owned_sabnzbd() {
    assert_eq!(
        std::env::var("LIBRARY_DISPOSABLE_SAB").as_deref(),
        Ok("I_ACCEPT_OWNED_QUEUE_WRITES")
    );
    let root = Path::new("/control");
    let meta = std::fs::symlink_metadata(root).unwrap();
    assert!(meta.is_dir() && meta.mode() & 0o077 == 0);
    let config_path = root.join("acceptance.json");
    let meta = std::fs::symlink_metadata(&config_path).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o077 == 0 && meta.len() < 4096);
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    assert!(!config.run_id.is_nil());
    assert_eq!(
        std::fs::read_link("/proc/self/ns/net")
            .unwrap()
            .to_str()
            .unwrap(),
        config.netns
    );
    assert!(!root.join("release-articles").exists());
    let scenario = async {
        let category = format!("owned-{}", config.run_id);
        let client_id = Uuid::new_v4();
        let own_id = Uuid::new_v4();
        let make = |key| {
            Sabnzbd::new(
                ClientConfig::new(
                    client_id,
                    BASE,
                    category.clone(),
                    HttpLimits {
                        timeout: Duration::from_secs(5),
                        max_response_bytes: 1024 * 1024,
                    },
                )
                .unwrap(),
                key,
            )
            .unwrap()
        };
        let client = make(config.key.clone());
        let version = loop {
            match client.test_connection().await {
                Ok(info) => break info.version,
                Err(ClientError::Unavailable) => tokio::time::sleep(POLL).await,
                Err(error) => panic!("fresh SAB connection failed: {error}"),
            }
        };
        assert_eq!(version, "5.1.3");
        assert!(
            matches!(
                make(config.nzb_key.clone()).test_connection().await,
                Err(ClientError::Authentication | ClientError::Rejected)
            ),
            "NZB key must not manage queues"
        );
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for mode in ["queue", "history"] {
            assert!(
                api(&http, &config.key, mode, None).await[mode]["slots"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let nzb = std::fs::read(root.join("owned.nzb")).unwrap();
        evidence(
            "attempted-fence.json",
            json!({"run_id":config.run_id,"own_id":own_id,"attempted":true}),
        );
        let mut attempt = SubmissionAttempt::from_persisted(own_id, false).unwrap();
        let jobs = client
            .enqueue(&mut attempt, AuthorizedPayload::nzb(nzb.clone()).unwrap())
            .await
            .expect("single enqueue; no retry");
        evidence("receipts.json", serde_json::to_value(&jobs).unwrap());
        assert_eq!(
            jobs.len(),
            1,
            "one NZB file must create one job; all returned receipts recorded"
        );
        let job = &jobs[0];
        assert_eq!(job.own_id(), own_id);
        let queue = api(&http, &config.key, "queue", Some(job.external_id())).await;
        let slots = queue["queue"]["slots"].as_array().unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0]["nzo_id"], job.external_id());
        assert_eq!(slots[0]["cat"], category);
        assert_eq!(slots[0]["filename"], format!("libraryd-{own_id}"));
        client.pause(job).await.expect("adapter pause");
        state(&client, job, DownloadState::Paused).await;
        client.resume(job).await.expect("adapter resume");
        loop {
            let status = client.status(job).await.unwrap().state;
            assert!(!matches!(
                status,
                DownloadState::Completed | DownloadState::Failed
            ));
            if matches!(status, DownloadState::Downloading | DownloadState::Queued) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("release-articles"))
            .unwrap()
            .sync_all()
            .unwrap();
        state(&client, job, DownloadState::Completed).await;
        let history = api(&http, &config.key, "history", Some(job.external_id())).await;
        let slots = history["history"]["slots"].as_array().unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0]["nzo_id"], job.external_id());
        assert_eq!(slots[0]["category"], category);
        assert_eq!(slots[0]["status"], "Completed");
        let paths = find_payload(Path::new("/downloads/complete"), 0);
        assert_eq!(paths.len(), 1);
        payload(&paths[0], &config.sha256);
        let served = std::fs::read_to_string(root.join("served.jsonl")).unwrap();
        for part in [1, 2] {
            assert!(
                served
                    .lines()
                    .any(|line| serde_json::from_str::<Value>(line).unwrap()["part"] == part)
            );
        }
        client
            .remove(job)
            .await
            .expect("history record removal preserving files");
        assert_eq!(client.status(job).await.unwrap_err(), ClientError::NotFound);
        assert!(
            api(&http, &config.key, "history", Some(job.external_id())).await["history"]["slots"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        payload(&paths[0], &config.sha256);
        assert_eq!(
            client
                .enqueue(&mut attempt, AuthorizedPayload::nzb(nzb).unwrap())
                .await
                .unwrap_err(),
            ClientError::NeedsReview
        );
        assert!(
            api(&http, &config.key, "queue", None).await["queue"]["slots"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        evidence(
            "result.json",
            json!({"passed":true,"run_id":config.run_id,"version":version,
            "sha256":config.sha256,"bytes":262144,"pause_resume_checked":true,"history_removed":true,"payload_preserved":true}),
        );
    };
    tokio::time::timeout(Duration::from_secs(180), scenario)
        .await
        .expect("owned SAB deadline");
}
