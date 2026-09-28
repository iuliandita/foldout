//! Synthetic loopback protocols only. Never contacts a configured external service.
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};

use crate::{
    settings::{CreateIntegration, IntegrationKind, SecretUpdate, Settings},
    store::sqlite::SqliteStore,
};

pub const NZB: &[u8] = b"<?xml version=\"1.0\"?><nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\"><file poster=\"fixture\" date=\"1\" subject=\"fixture.cbz\"><groups><group>alt.test</group></groups><segments><segment bytes=\"1\" number=\"1\">owned-fixture</segment></segments></file></nzb>";

pub fn private_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[derive(Default)]
pub struct Seen {
    pub requests: Vec<String>,
    pub submissions: usize,
    pub drop_response: bool,
    pub completed: bool,
    pub category: Option<String>,
    pub own_tag: String,
    pub expected_hash: String,
    pub torrent: Vec<u8>,
    pub torrent_mode: bool,
    pub payload_started: Option<Arc<Notify>>,
    pub payload_release: Option<Arc<Notify>>,
}
pub struct Server {
    pub url: String,
    pub seen: Arc<Mutex<Seen>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(store: SqliteStore) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let base = url.clone();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let state = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 65536);
                }
                // Every tested client sends a known bounded Content-Length.
                let Some(header_end) = request
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|n| n + 4)
                else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                }
                let first = head.lines().next().unwrap();
                let target = first.split_whitespace().nth(1).unwrap();
                let uri = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                let form = String::from_utf8_lossy(&request[header_end..]);
                let mutation = (uri.path() == "/api" && form.contains("addfile"))
                    || uri.path() == "/api/v2/torrents/add";
                if uri.path().starts_with("/payload/") {
                    let (started, release) = {
                        let seen = state.lock().unwrap();
                        (seen.payload_started.clone(), seen.payload_release.clone())
                    };
                    if let Some(started) = started {
                        started.notify_one();
                    }
                    if let Some(release) = release {
                        release.notified().await;
                    }
                }
                if mutation {
                    let fenced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisition_runs WHERE attempted = 1 AND state = 'needs_review' AND payload_digest IS NOT NULL").fetch_one(store.reader()).await.unwrap();
                    assert!(fenced > 0, "remote mutation occurred before durable fence");
                }
                let reply = {
                    let mut seen = state.lock().unwrap();
                    seen.requests.push(first.to_string());
                    if mutation {
                        seen.submissions += 1;
                    }
                    if mutation && seen.drop_response {
                        None
                    } else {
                        let bytes = if uri.path().ends_with("/api") && uri.path() != "/api" {
                            let params: std::collections::HashMap<_, _> =
                                uri.query_pairs().collect();
                            assert!(uri.path() == "/7/api" || uri.path() == "/8/api");
                            if params.get("t").unwrap() == "caps" {
                                b"<caps><limits max=\"100\"/><searching><search available=\"yes\" supportedParams=\"q\"/></searching><categories><category id=\"7030\"/><category id=\"7020\"/><category id=\"7010\"/></categories></caps>".to_vec()
                            } else {
                                let category = params.get("cat").unwrap();
                                let query = params.get("q").unwrap();
                                let offset = params.get("offset").unwrap();
                                let id = if uri.path() == "/7/api" { 7 } else { 8 };
                                let mime = if seen.torrent_mode {
                                    "application/x-bittorrent"
                                } else {
                                    "application/x-nzb"
                                };
                                format!("<rss xmlns:newznab=\"http://www.newznab.com/DTD/2010/feeds/attributes/\"><channel><newznab:response offset=\"{offset}\" total=\"1\"/><item><title>{query}</title><guid>https://guid.invalid/selected/opaque-guid</guid><enclosure url=\"{base}payload/{id}?token=payload-secret\" length=\"512\" type=\"{mime}\"/><newznab:attr name=\"category\" value=\"{category}\"/></item></channel></rss>").into_bytes()
                            }
                        } else if uri.path().starts_with("/payload/") {
                            if seen.torrent.is_empty() {
                                NZB.to_vec()
                            } else {
                                seen.torrent.clone()
                            }
                        } else if uri.path() == "/api" {
                            if mutation {
                                b"{\"status\":true,\"nzo_ids\":[\"SABnzbd_nzo_one\",\"SABnzbd_nzo_two\"]}".to_vec()
                            } else {
                                let params: std::collections::HashMap<_, _> =
                                    reqwest::Url::parse(&format!("http://localhost/?{form}"))
                                        .unwrap()
                                        .query_pairs()
                                        .map(|(k, v)| (k.to_string(), v.to_string()))
                                        .collect();
                                let mode = params.get("mode").unwrap();
                                let id = params.get("nzo_ids").unwrap();
                                let category = seen.category.as_deref().unwrap_or("libraryd");
                                if mode == "queue" && seen.completed {
                                    b"{\"queue\":{\"slots\":[]}}".to_vec()
                                } else {
                                    serde_json::json!({mode:{"slots":[{"nzo_id":id,"cat":category,"status":if seen.completed {"Completed"}else{"Downloading"},"storage":"/private/remote/path"}]}}).to_string().into_bytes()
                                }
                            }
                        } else if uri.path() == "/api/v2/auth/login" {
                            b"Ok.".to_vec()
                        } else if uri.path() == "/api/v2/app/version" {
                            b"v5.2.3".to_vec()
                        } else if uri.path() == "/api/v2/torrents/add" {
                            b"Ok.".to_vec()
                        } else if uri.path() == "/api/v2/torrents/info" {
                            if seen.submissions == 0 {
                                b"[]".to_vec()
                            } else {
                                serde_json::json!([{"hash":seen.expected_hash,"category":seen.category.as_deref().unwrap_or("libraryd"),"tags":seen.own_tag,"state":if seen.completed {"uploading"}else{"downloading"}}]).to_string().into_bytes()
                            }
                        } else {
                            panic!("unexpected fixture request: {first}")
                        };
                        Some(bytes)
                    }
                };
                if let Some(reply) = reply {
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nSet-Cookie: SID=fixture; Path=/; HttpOnly\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.len()
                    );
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(&reply).await.unwrap();
                }
            }
        });
        Self { url, seen, task }
    }
}

pub async fn integration(
    settings: &Settings,
    server: &Server,
    kind: IntegrationKind,
    indexer_id: u32,
    torrent: bool,
) -> String {
    if kind == IntegrationKind::Prowlarr {
        server.seen.lock().unwrap().torrent_mode = torrent;
    }
    let options = match kind {
        IntegrationKind::Prowlarr => {
            serde_json::json!({"indexer_id":indexer_id,"protocol":if torrent {"torrent"}else{"usenet"},"categories":{"comics":[7030],"manga":[7020],"magazines":[7010]}})
        }
        _ => serde_json::json!({"category":"libraryd"}),
    };
    settings
        .create(CreateIntegration {
            kind,
            label: "fixture".into(),
            base_url: server.url.clone(),
            enabled: true,
            options,
            api_key: SecretUpdate::Set("fixture-private-key".into()),
            username: SecretUpdate::Set("fixture-user".into()),
            password: SecretUpdate::Set("fixture-password".into()),
        })
        .await
        .unwrap()
        .id
}
pub async fn allow_search(store: &SqliteStore) {
    let mut tx = store.begin_write().await.unwrap();
    sqlx::query("UPDATE search_cooldowns SET next_at = 0")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
