//! Opt-in real SAB transfer through public services; no production configuration.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use libraryd::{
    acquisition::pipeline::{
        AcquisitionReason, AcquisitionRequest, AcquisitionState, DestinationSelection,
        FileAssociation, Pipeline,
    },
    catalog::{CatalogRepository, ContentType, NewEdition, NewPublication, NewUnit, UnitKind},
    clients::{ClientConfig, ClientError, HttpLimits, Sabnzbd},
    importer::journal::{ImportPhase, ImportService},
    library::roots::Library,
    reader::service::ReaderService,
    search::{
        ReleaseSearch, Search,
        matching::Eligibility,
        selection::{DecisionAction, NewDecision, SelectionRepository},
    },
    settings::{CreateIntegration, EncryptionKey, IntegrationKind, SecretUpdate, Settings},
    store::sqlite::SqliteStore,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const SAB: &str = "http://127.0.0.1:8080/";
const POLL: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    run_id: Uuid,
    key: String,
    nzb_key: String,
    sha256: String,
    netns: String,
}

#[derive(Clone)]
struct Indexer {
    base: String,
    case: &'static str,
    category: u32,
    nzb: Arc<Vec<u8>>,
    requests: Arc<Mutex<Vec<String>>>,
}
struct Server {
    state: Indexer,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(case: &'static str, category: u32, nzb: Vec<u8>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Indexer {
            base: format!("http://{}/", listener.local_addr().unwrap()),
            case,
            category,
            nzb: Arc::new(nzb),
            requests: Arc::default(),
        };
        let router = Router::new().fallback(indexer).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { state, task }
    }
}
async fn indexer(State(state): State<Indexer>, request: Request<Body>) -> Response<Body> {
    let uri = request.uri();
    let url = reqwest::Url::parse(&format!("http://fixture.invalid{uri}")).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    let response = |status, content: String| {
        Response::builder()
            .status(status)
            .header("content-type", "application/xml")
            .body(Body::from(content))
            .unwrap()
    };
    if request.method() != "GET" || uri.to_string().len() > 4096 {
        return response(StatusCode::BAD_REQUEST, String::new());
    }
    let mut seen = state.requests.lock().unwrap();
    if seen.len() >= 24 {
        return response(StatusCode::TOO_MANY_REQUESTS, String::new());
    }
    // Record paths only: no API key or response body in public diagnostics.
    seen.push(url.path().into());
    drop(seen);
    if url.path() == "/payload" {
        return Response::builder()
            .header("content-type", "application/x-nzb")
            .body(Body::from(state.nzb.as_ref().clone()))
            .unwrap();
    }
    if url.path() != "/7/api"
        || params.get("apikey").map(|v| v.as_ref()) != Some("owned-indexer-key")
    {
        return response(StatusCode::NOT_FOUND, String::new());
    }
    match params.get("t").map(|v| v.as_ref()) {
        Some("caps") => response(
            StatusCode::OK,
            format!(
                "<caps><limits max=\"100\"/><searching><search available=\"yes\" supportedParams=\"q\"/></searching><categories><category id=\"{}\"/></categories></caps>",
                state.category
            ),
        ),
        Some("search")
            if params.get("cat").map(|v| v.as_ref())
                == Some(state.category.to_string().as_str())
                && params.get("q").map(|v| v.as_ref()) == Some(state.case)
                && params.get("offset").map(|v| v.as_ref()) == Some("0") =>
        {
            response(
                StatusCode::OK,
                format!(
                    "<rss xmlns:newznab=\"http://www.newznab.com/DTD/2010/feeds/attributes/\"><channel><newznab:response offset=\"0\" total=\"1\"/><item><title>{}</title><guid>https://fixture.invalid/{}</guid><enclosure url=\"{}payload\" length=\"{}\" type=\"application/x-nzb\"/><newznab:attr name=\"category\" value=\"{}\"/></item></channel></rss>",
                    state.case,
                    state.case,
                    state.base,
                    state.nzb.len(),
                    state.category
                ),
            )
        }
        _ => response(StatusCode::BAD_REQUEST, String::new()),
    }
}

fn private_dir(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn evidence(path: &Path, value: Value) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer(&file, &value).unwrap();
    file.sync_all().unwrap();
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn find_source(root: &Path, filename: &str, depth: usize) -> Vec<PathBuf> {
    assert!(depth <= 5);
    let entries: Vec<_> = std::fs::read_dir(root).unwrap().collect();
    assert!(entries.len() <= 32);
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        if kind.is_dir() {
            found.extend(find_source(&entry.path(), filename, depth + 1));
        } else if kind.is_file() && entry.file_name() == filename {
            found.push(entry.path());
        }
    }
    found
}
async fn advance(pipeline: &Pipeline, settings: &Settings, id: &str, expected: AcquisitionState) {
    loop {
        pipeline.tick_job(settings.clone(), id).await.unwrap();
        let view = pipeline.get("owned-acceptance", id).await.unwrap();
        assert_ne!(view.state, AcquisitionState::NeedsReview, "{view:?}");
        if view.state == expected {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}
async fn integration(
    settings: &Settings,
    kind: IntegrationKind,
    base: String,
    key: String,
    options: Value,
) -> String {
    settings
        .create(CreateIntegration {
            kind,
            label: "owned acceptance".into(),
            base_url: base,
            enabled: true,
            options,
            api_key: SecretUpdate::Set(key),
            username: SecretUpdate::Clear,
            password: SecretUpdate::Clear,
        })
        .await
        .unwrap()
        .id
}

async fn run_case(
    config: &Config,
    case: &'static str,
    content: ContentType,
    extension: &str,
    category: u32,
    expected: &[u8],
    pages: usize,
) -> Value {
    let control = Path::new("/control").join(case);
    let root = Path::new("/config").join(case);
    private_dir(&root);
    let state_dir = root.join("state");
    let destination = root.join("library");
    private_dir(&state_dir);
    private_dir(&destination);
    let filename = format!("{case}.{extension}");
    assert_eq!(std::fs::read(control.join(&filename)).unwrap(), expected);
    assert!(!control.join("release-articles").exists());
    let server = Server::start(
        case,
        category,
        std::fs::read(control.join("owned.nzb")).unwrap(),
    )
    .await;
    let key = EncryptionKey::load_or_create(&state_dir).await.unwrap();
    let store = SqliteStore::open(&state_dir).await.unwrap();
    let settings = Settings::new(store.clone(), key);
    let source_integration = integration(&settings, IntegrationKind::Prowlarr, server.state.base.clone(),
        "owned-indexer-key".into(), json!({"indexer_id":7,"protocol":"usenet","categories":{"comics":[7030],"manga":[7020],"magazines":[7010]}})).await;
    let client_id = integration(
        &settings,
        IntegrationKind::Sabnzbd,
        SAB.into(),
        config.key.clone(),
        json!({"category":format!("owned-{}",config.run_id)}),
    )
    .await;
    let library = Library::new(store.clone());
    let download_root = library
        .register_root("owned download", Path::new("/downloads"))
        .await
        .unwrap();
    let destination_root = library
        .register_root("owned library", &destination)
        .await
        .unwrap();
    let catalog = CatalogRepository::new(store.clone());
    let publication = catalog
        .create_publication(NewPublication {
            content_type: content.clone(),
            title: case.into(),
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
            label: if case == "magazine" { "7-8" } else { "12.5" }.into(),
            kind: if case == "magazine" {
                UnitKind::Combined
            } else {
                UnitKind::Issue
            },
            sort_key: None,
            date: None,
        })
        .await
        .unwrap();
    let search = Search::new(store.clone(), settings.clone());
    let releases = search
        .releases(
            "owned-acceptance",
            ReleaseSearch {
                integration_id: source_integration,
                content_type: content,
                query: case.into(),
                unit_id: Some(unit.id.clone()),
                offset: 0,
                limit: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(releases.releases.len(), 1);
    let assessed = &releases.releases[0];
    let assessment_id = assessed.assessment_id.clone().unwrap();
    let decision = SelectionRepository::new(store.clone())
        .decide(
            "owned-acceptance",
            &format!("{case}-selection"),
            NewDecision {
                assessment_id: assessment_id.clone(),
                action: DecisionAction::Selected,
                acknowledged_assessment_id: (assessed.evaluation.as_ref().unwrap().eligibility
                    == Eligibility::Unknown)
                    .then_some(assessment_id),
                reason: None,
            },
        )
        .await
        .unwrap();
    let pipeline = Pipeline::new(store.clone());
    let request = AcquisitionRequest {
        release_handle: releases.releases[0].release_handle.clone(),
        client_id,
        unit_id: unit.id.clone(),
        selection_decision_id: Some(decision.id),
        destination: Some(DestinationSelection {
            root_id: destination_root.id,
            relative_path: filename.clone(),
        }),
    };
    let intent = pipeline
        .create(&settings, "owned-acceptance", case, request.clone())
        .await
        .unwrap();
    advance(
        &pipeline,
        &settings,
        &intent.id,
        AcquisitionState::Downloading,
    )
    .await;
    let accepted = pipeline.get("owned-acceptance", &intent.id).await.unwrap();
    assert!(accepted.submitted);
    assert_eq!(accepted.receipt_count, 1);
    let receipts: Vec<String> = sqlx::query_scalar(
        "SELECT external_id FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal",
    )
    .bind(&intent.id)
    .fetch_all(store.reader())
    .await
    .unwrap();
    assert_eq!(receipts.len(), 1);
    evidence(
        &control.join("receipts.json"),
        json!({"acquisition_id":intent.id,"receipts":receipts}),
    );
    let reader = ReaderService::new(store.clone());
    assert!(reader.files(&unit.id).await.unwrap().is_empty());
    assert!(!destination.join(&filename).exists());
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(control.join("release-articles"))
        .unwrap()
        .sync_all()
        .unwrap();
    advance(
        &pipeline,
        &settings,
        &intent.id,
        AcquisitionState::Downloaded,
    )
    .await;
    assert_eq!(
        pipeline
            .get("owned-acceptance", &intent.id)
            .await
            .unwrap()
            .reason,
        Some(AcquisitionReason::FileAssociationRequired)
    );
    assert!(reader.files(&unit.id).await.unwrap().is_empty());
    assert!(!destination.join(&filename).exists());
    let sources = find_source(Path::new("/downloads/complete"), &filename, 0);
    assert_eq!(sources.len(), 1);
    assert_eq!(std::fs::read(&sources[0]).unwrap(), expected);
    let served = std::fs::read_to_string(control.join("served.jsonl")).unwrap();
    assert!(served.len() < 16 * 1024);
    for part in [1, 2] {
        assert!(
            served
                .lines()
                .any(|line| { serde_json::from_str::<Value>(line).unwrap()["part"] == part })
        );
    }
    pipeline
        .associate_file(
            "owned-acceptance",
            &intent.id,
            FileAssociation {
                source_root_id: download_root.id,
                source_relative_path: sources[0]
                    .strip_prefix("/downloads")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                destination: None,
            },
        )
        .await
        .unwrap();
    advance(
        &pipeline,
        &settings,
        &intent.id,
        AcquisitionState::Completed,
    )
    .await;
    let complete = pipeline.get("owned-acceptance", &intent.id).await.unwrap();
    let journal = ImportService::new(store.clone())
        .get(complete.import_id.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(journal.phase, ImportPhase::Done);
    assert_eq!(journal.effective_policy.as_deref(), Some("copy"));
    assert_eq!(std::fs::read(&sources[0]).unwrap(), expected);
    let imported = std::fs::read(destination.join(&filename)).unwrap();
    assert_eq!(imported, expected);
    let source_sha256 = digest(&std::fs::read(&sources[0]).unwrap());
    let imported_sha256 = digest(&imported);
    assert_eq!(source_sha256, digest(expected));
    assert_eq!(imported_sha256, source_sha256);
    let files = reader.files(&unit.id).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        journal.library_file_id.as_deref(),
        Some(files[0].id.as_str())
    );
    let manifest = reader.manifest(&files[0].id).await.unwrap();
    assert_eq!(manifest.format, extension);
    assert_eq!(manifest.page_count, pages);
    let page = reader.page(&files[0].id, 0).await.unwrap();
    assert!(!page.bytes.is_empty() && page.width > 0 && page.height > 0);
    if extension == "cbz" {
        assert_eq!((page.width, page.height), (16, 24));
    }
    assert_eq!(
        pipeline
            .create(&settings, "owned-acceptance", case, request)
            .await
            .unwrap()
            .id,
        intent.id
    );
    let requests = server.state.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().filter(|p| p.as_str() == "/payload").count(),
        1
    );
    store.close().await;
    let reopened = SqliteStore::open(&state_dir).await.unwrap();
    let reopened_receipts: Vec<String> = sqlx::query_scalar(
        "SELECT external_id FROM acquisition_receipts WHERE acquisition_id = ? ORDER BY ordinal",
    )
    .bind(&intent.id)
    .fetch_all(reopened.reader())
    .await
    .unwrap();
    assert_eq!(reopened_receipts, receipts);
    assert_eq!(
        Pipeline::new(reopened.clone())
            .get("owned-acceptance", &intent.id)
            .await
            .unwrap()
            .state,
        AcquisitionState::Completed
    );
    let reopened_reader = ReaderService::new(reopened.clone());
    let reopened_journal = ImportService::new(reopened.clone())
        .get(complete.import_id.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(reopened_journal.phase, ImportPhase::Done);
    assert_eq!(reopened_journal.library_file_id, journal.library_file_id);
    assert_eq!(reopened_reader.files(&unit.id).await.unwrap().len(), 1);
    assert_eq!(
        reopened_reader
            .manifest(&files[0].id)
            .await
            .unwrap()
            .page_count,
        pages
    );
    reopened.close().await;
    let result = json!({"case":case,"passed":true,"acquisition_id":intent.id,"unit_id":unit.id,
        "sha256":imported_sha256,"source_sha256":source_sha256,"bytes":expected.len(),"receipts":receipts.len(),
        "journal":"done","source_preserved":true,"explicit_association":true,"reader_pages":pages,
        "decoded_width":page.width,"decoded_height":page.height,"reopen_verified":true});
    evidence(&control.join("result.json"), result.clone());
    result
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires check-sabnzbd.py --pipeline and owned queue-write opt-in"]
async fn disposable_owned_sabnzbd_pipeline() {
    // Absolute nice value; inherited negative priorities must not offset this.
    assert_eq!(unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 8) }, 0);
    unsafe {
        libc::umask(0o077);
    }
    assert_eq!(
        std::env::var("LIBRARY_DISPOSABLE_SAB").as_deref(),
        Ok("I_ACCEPT_OWNED_QUEUE_WRITES")
    );
    for directory in ["/control", "/config"] {
        let meta = std::fs::symlink_metadata(directory).unwrap();
        assert!(meta.is_dir() && meta.mode() & 0o077 == 0);
    }
    for binary in [
        "/usr/bin/7z",
        "/usr/bin/prlimit",
        "/usr/bin/pdfinfo",
        "/usr/bin/pdftoppm",
    ] {
        assert!(
            std::fs::metadata(binary).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0),
            "reader prerequisite missing: {binary}"
        );
    }
    let config_path = Path::new("/control/acceptance.json");
    let meta = std::fs::symlink_metadata(config_path).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o077 == 0 && meta.len() < 4096);
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    assert!(!config.run_id.is_nil() && config.key != config.nzb_key && config.sha256.len() == 64);
    assert_eq!(
        std::fs::read_link("/proc/self/ns/net")
            .unwrap()
            .to_str()
            .unwrap(),
        config.netns
    );
    let scenario = async {
        let sab = Sabnzbd::new(
            ClientConfig::new(
                Uuid::new_v4(),
                SAB,
                format!("owned-{}", config.run_id),
                HttpLimits {
                    timeout: Duration::from_secs(5),
                    max_response_bytes: 1024 * 1024,
                },
            )
            .unwrap(),
            config.key.clone(),
        )
        .unwrap();
        loop {
            match sab.test_connection().await {
                Ok(info) => {
                    assert_eq!(info.version, "5.1.3");
                    break;
                }
                Err(ClientError::Unavailable) => tokio::time::sleep(POLL).await,
                Err(error) => panic!("owned SAB readiness failed: {error}"),
            }
        }
        let mut results = Vec::new();
        for (case, content, extension, category, bytes, pages) in [
            (
                "comic",
                ContentType::Comic,
                "cbz",
                7030,
                include_bytes!("fixtures/natural-order.cbz").as_slice(),
                3,
            ),
            (
                "manga",
                ContentType::Manga,
                "cbz",
                7020,
                include_bytes!("fixtures/natural-order.cbz").as_slice(),
                3,
            ),
            (
                "magazine",
                ContentType::Magazine,
                "pdf",
                7010,
                include_bytes!("fixtures/single-page.pdf").as_slice(),
                1,
            ),
        ] {
            results.push(
                tokio::time::timeout(
                    Duration::from_secs(180),
                    run_case(&config, case, content, extension, category, bytes, pages),
                )
                .await
                .expect("180-second owned pipeline case deadline"),
            );
        }
        evidence(
            Path::new("/control/result.json"),
            json!({"passed":true,"run_id":config.run_id,"cases":results}),
        );
    };
    tokio::time::timeout(Duration::from_secs(560), scenario)
        .await
        .expect("owned pipeline total deadline");
}
