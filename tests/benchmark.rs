//! Opt-in owned-data benchmark. Run it through scripts/benchmark, which records results in an ignored local file.
#![cfg(target_os = "linux")]

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use libraryd::{
    app,
    auth::{AuthService, Scope},
    catalog::CatalogRepository,
    importer::preview::PreviewService,
    library::{roots::Library, scan::ScanReport},
    reader::service::ReaderService,
    settings::{EncryptionKey, Settings},
    store::sqlite::SqliteStore,
};
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;

const TARGET: usize = 50_000;
const BATCH: usize = 500;
const RESERVED: usize = 20;
const PAGE: u32 = 100;
const DISK_CAP: u64 = 16_000_000_000;
const SAMPLE_CAP: u64 = 48 * 1024 * 1024;
const RSS_CAP: u64 = 2 * 1024 * 1024 * 1024;
const ORIGIN: &str = "http://127.0.0.1:8787";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Timings = BTreeMap<String, Vec<f64>>;

fn record(times: &mut Timings, name: &str, start: Instant) {
    times
        .entry(name.into())
        .or_default()
        .push(start.elapsed().as_secs_f64() * 1000.0);
}

fn summary(times: Timings) -> Value {
    let mut result = serde_json::Map::new();
    for (name, mut values) in times {
        values.sort_by(f64::total_cmp);
        let quantile = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
        let p95 = quantile(95);
        result.insert(
            name,
            json!({"count":values.len(), "p50_ms":quantile(50),
            "p95_ms":p95, "max_ms":values[values.len()-1],
            "aspirational_300ms_p95_met":p95 < 300.0,
            "aspirational_1000ms_p95_met":p95 < 1000.0}),
        );
    }
    Value::Object(result)
}

fn relative(index: usize) -> String {
    let (kind, extension) = match index % 10 {
        0..=4 => ("comic", "cbz"),
        5..=7 => ("manga", "cbz"),
        _ => ("magazine", "pdf"),
    };
    format!("{kind}/{:03}/{index:05}.{extension}", index / 1000)
}

fn owned_corpus(root: &Path, count: usize) -> Result<Value> {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["natural-order.cbz", "single-page.pdf"] {
        let metadata = fs::symlink_metadata(fixture_dir.join(name))?;
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert!(metadata.len() <= SAMPLE_CAP, "fixture exceeds 48 MiB");
    }
    let cbz = fs::read(fixture_dir.join("natural-order.cbz"))?;
    let pdf = fs::read(fixture_dir.join("single-page.pdf"))?;
    assert!(!cbz.is_empty() && !pdf.is_empty());
    assert!(
        cbz.len().max(pdf.len()) as u64 <= SAMPLE_CAP,
        "fixture exceeds 48 MiB"
    );
    // Reserve two GiB for state/WAL and use rounded filesystem blocks for the corpus.
    let estimated = count as u64 * (cbz.len().max(pdf.len()) as u64).div_ceil(4096) * 4096
        + 2 * 1024 * 1024 * 1024;
    assert!(
        estimated < DISK_CAP,
        "projected benchmark footprint exceeds 16 GB"
    );
    let mut distribution = BTreeMap::<String, (usize, u64)>::new();
    let mut inodes = HashSet::new();
    let mut allocated = 0;
    for index in 0..count {
        let name = relative(index);
        let path = root.join(&name);
        fs::create_dir_all(path.parent().unwrap())?;
        let bytes = if name.ends_with(".pdf") { &pdf } else { &cbz };
        // write_all creates independent storage: no hard links, reflinks, or sparse files.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        let metadata = file.metadata()?;
        assert!(metadata.is_file() && metadata.nlink() == 1);
        assert!(inodes.insert((metadata.dev(), metadata.ino())));
        assert_eq!(metadata.len(), bytes.len() as u64);
        allocated += metadata.blocks() * 512;
        let item = distribution
            .entry(name.split('/').next().unwrap().into())
            .or_default();
        item.0 += 1;
        item.1 += metadata.len();
    }
    let total_bytes: u64 = distribution.values().map(|(_, bytes)| bytes).sum();
    Ok(
        json!({"files":count, "distinct_regular_inodes":inodes.len(),
        "distribution_count_bytes":distribution, "total_bytes":total_bytes,
        "allocated_file_bytes":allocated, "cbz_sample_bytes":cbz.len(), "pdf_sample_bytes":pdf.len(),
        "largest_sample_bytes":cbz.len().max(pdf.len()), "estimated_budget_bytes":estimated}),
    )
}

async fn scan(library: &Library, store: &SqliteStore, root: &str, count: usize) -> Result<Value> {
    let start = Instant::now();
    let report: ScanReport = library.scan(root).await?;
    let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(report.visited, count as u64);
    assert_eq!((report.errors, report.skipped), (0, 0));
    let rows = sqlx::query("SELECT COALESCE(reason,'none') AS reason, COUNT(*) AS count FROM scan_entries WHERE last_seen_run_id=? GROUP BY reason")
        .bind(&report.run_id).fetch_all(store.reader()).await?;
    let reasons: BTreeMap<String, i64> = rows
        .iter()
        .map(|row| (row.get("reason"), row.get("count")))
        .collect();
    Ok(json!({"elapsed_ms":milliseconds,"report":report,"reason_counts":reasons}))
}

async fn seed(store: &SqliteStore, root: &Path, root_id: &str, count: usize) -> Result<()> {
    // One complete catalog tuple per file makes all three content-type lists substantial.
    // Reserved files get catalog units but no file/coverage row until the mixed import load.
    for start in (0..count).step_by(BATCH) {
        let mut tx = store.begin_write().await?;
        for index in start..(start + BATCH).min(count) {
            let id = format!("bench-{index:05}");
            let name = relative(index);
            let kind = name.split('/').next().unwrap();
            sqlx::query("INSERT INTO publications(id,content_type,title,sort_title,known_unit_count) VALUES(?,?,?,?,1)")
                .bind(&id).bind(kind).bind(&id).bind(&id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO editions(id,publication_id,language) VALUES(?,?,'en')")
                .bind(&id)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO units(id,edition_id,label,kind,sort_key) VALUES(?,?,'1',?,'00001')",
            )
            .bind(&id)
            .bind(&id)
            .bind(if kind == "manga" { "chapter" } else { "issue" })
            .execute(&mut *tx)
            .await?;
            if index < count - RESERVED {
                let inserted = sqlx::query("INSERT INTO library_files(id,path,format,signature,size_bytes,root_id,relative_path,mtime_ns) SELECT ?,?,format,signature,size_bytes,root_id,relative_path,mtime_ns FROM scan_entries WHERE root_id=? AND relative_path=? AND state='pending_association'")
                    .bind(&id).bind(root.join(&name).to_str().unwrap()).bind(root_id).bind(&name).execute(&mut *tx).await?;
                assert_eq!(inserted.rows_affected(), 1);
                sqlx::query("INSERT INTO file_coverage(library_file_id,unit_id,evidence) VALUES(?,?,'user_confirmed')")
                    .bind(&id).bind(&id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
    }
    for query in [
        "SELECT COUNT(*) FROM publications",
        "SELECT COUNT(*) FROM editions",
        "SELECT COUNT(*) FROM units",
    ] {
        let actual: i64 = sqlx::query_scalar(query).fetch_one(store.reader()).await?;
        assert_eq!(actual, count as i64, "{query}");
    }
    Ok(())
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    input: Value,
    status: StatusCode,
) -> Result<Vec<u8>> {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(input.to_string()))?;
    let response =
        tokio::time::timeout(Duration::from_secs(15), router.clone().oneshot(request)).await??;
    assert_eq!(response.status(), status, "{method} {path}");
    let bytes = response.into_body().collect().await?.to_bytes();
    assert!(bytes.len() <= 32 * 1024 * 1024);
    Ok(bytes.to_vec())
}

async fn session(
    store: &SqliteStore,
    router: &Router,
    token: &str,
    count: usize,
    reader_index: usize,
    reader: Option<&ReaderService>,
) -> Result<Timings> {
    let catalog = CatalogRepository::new(store.clone());
    let mut times = Timings::new();
    let mut db_cursor = None;
    let mut api_cursor: Option<String> = None;
    let mut db_count = 0;
    let mut api_count = 0;
    let mut page_number = 0;
    loop {
        let start = Instant::now();
        let page = catalog
            .list_publications(PAGE, db_cursor.as_deref(), None)
            .await?;
        record(&mut times, "catalog_page", start);
        assert!(!page.items.is_empty());
        db_count += page.items.len();
        let path = match &api_cursor {
            Some(cursor) => format!("/api/v1/publications?limit={PAGE}&cursor={cursor}"),
            None => format!("/api/v1/publications?limit={PAGE}"),
        };
        let start = Instant::now();
        let bytes = request(router, "GET", &path, token, Value::Null, StatusCode::OK).await?;
        record(&mut times, "api_catalog_page", start);
        let value: Value = serde_json::from_slice(&bytes)?;
        let items = value["items"].as_array().unwrap();
        assert_eq!(items.len(), page.items.len());
        for (api, db) in items.iter().zip(&page.items) {
            assert_eq!(api["id"].as_str(), Some(db.id.as_str()));
        }
        api_count += items.len();
        let publication = &page.items[0].id;
        let start = Instant::now();
        let editions = catalog.list_editions_page(publication, PAGE, None).await?;
        let units = catalog
            .list_units_page(&editions.items[0].id, PAGE, None)
            .await?;
        assert_eq!((editions.items.len(), units.items.len()), (1, 1));
        record(&mut times, "catalog_edition_unit_pages", start);
        if let Some(reader) = reader {
            // Rotate all three content types, with PDF indices 8/9 in each ten-file group.
            let index = (page_number * 5 + reader_index) % (count - RESERVED);
            let file = format!("bench-{index:05}");
            let start = Instant::now();
            let bytes = reader.page(&file, 0).await?;
            assert!(!bytes.bytes.is_empty());
            record(&mut times, "reader_service_page", start);
            let start = Instant::now();
            let bytes = request(
                router,
                "GET",
                &format!("/api/v1/library/files/{file}/pages/0"),
                token,
                Value::Null,
                StatusCode::OK,
            )
            .await?;
            assert!(!bytes.is_empty());
            record(&mut times, "api_reader_page", start);
        }
        page_number += 1;
        db_cursor = page.next_cursor;
        api_cursor = value["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(db_cursor.is_some(), api_cursor.is_some());
        if db_cursor.is_none() {
            break;
        }
        assert!(page_number <= count.div_ceil(PAGE as usize));
    }
    assert_eq!((db_count, api_count), (count, count));
    Ok(times)
}

async fn five_sessions(
    store: &SqliteStore,
    router: &Router,
    tokens: &[String],
    count: usize,
    mixed: bool,
) -> Result<Timings> {
    let reader = ReaderService::new(store.clone());
    let reader = mixed.then_some(&reader);
    let results = tokio::try_join!(
        session(store, router, &tokens[0], count, 0, reader),
        session(store, router, &tokens[1], count, 1, reader),
        session(store, router, &tokens[2], count, 2, reader),
        session(store, router, &tokens[3], count, 3, reader),
        session(store, router, &tokens[4], count, 4, reader),
    )?;
    let mut combined = Timings::new();
    for times in [results.0, results.1, results.2, results.3, results.4] {
        for (name, values) in times {
            combined.entry(name).or_default().extend(values);
        }
    }
    Ok(combined)
}

async fn writes(
    store: &SqliteStore,
    router: &Router,
    token: &str,
    settings: &Settings,
    root: &str,
    count: usize,
) -> Result<Timings> {
    let mut times = Timings::new();
    let previews = PreviewService::new(store.clone());
    for index in count - RESERVED..count {
        let start = Instant::now();
        let entry: String =
            sqlx::query_scalar("SELECT id FROM scan_entries WHERE root_id=? AND relative_path=?")
                .bind(root)
                .bind(relative(index))
                .fetch_one(store.reader())
                .await?;
        let preview = previews
            .preview(&entry, &format!("bench-{index:05}"))
            .await?;
        let file = previews.accept(&preview.id).await?;
        assert!(file.size_bytes > 0);
        record(&mut times, "import_preview_accept", start);

        let start = Instant::now();
        let bytes = request(
            router,
            "POST",
            "/api/v1/auth/keys",
            token,
            json!({"name":format!("mixed-{index}"),"scope":"read"}),
            StatusCode::CREATED,
        )
        .await?;
        record(&mut times, "auth_key_save", start);
        let key: Value = serde_json::from_slice(&bytes)?;
        let new_token = key["secret"].as_str().unwrap();
        let start = Instant::now();
        request(
            router,
            "GET",
            "/api/v1/publications?limit=1",
            new_token,
            Value::Null,
            StatusCode::OK,
        )
        .await?;
        record(&mut times, "auth_new_key_read", start);

        let secret = format!("owned-benchmark-secret-{index}");
        let start = Instant::now();
        let bytes = request(
            router,
            "POST",
            "/api/v1/settings/integrations",
            token,
            json!({"kind":"prowlarr","label":format!("benchmark-{index}"),
                "base_url":"http://127.0.0.1:9/","enabled":false,"api_key":secret,
                "options":{"indexer_id":1,"protocol":"usenet","categories":{}}}),
            StatusCode::CREATED,
        )
        .await?;
        let integration: Value = serde_json::from_slice(&bytes)?;
        assert!(!String::from_utf8_lossy(&bytes).contains(&secret));
        assert_eq!(integration["credentials_configured"], true);
        let id = integration["id"].as_str().unwrap();
        // get() decrypts and authenticates the stored ciphertext before returning its safe view.
        let reopened = settings.get(id).await?;
        assert_eq!(reopened.id, id);
        let ciphertext: Vec<u8> =
            sqlx::query_scalar("SELECT secret_ciphertext FROM integrations WHERE id=?")
                .bind(id)
                .fetch_one(store.reader())
                .await?;
        assert!(
            !ciphertext
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
        record(&mut times, "encrypted_credential_save_readback", start);
    }
    Ok(times)
}

// Read only /proc metadata for this process and descendants, never names or command lines.
fn rss_tree(pid: u32, seen: &mut HashSet<u32>) -> u64 {
    if !seen.insert(pid) {
        return 0;
    }
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let mut rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|line| line.split_whitespace().next()?.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024;
    if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
        for task in tasks.flatten() {
            if let Ok(children) = fs::read_to_string(task.path().join("children")) {
                for child in children
                    .split_whitespace()
                    .filter_map(|value| value.parse().ok())
                {
                    rss += rss_tree(child, seen);
                }
            }
        }
    }
    rss
}

fn disk_bytes(root: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        assert!(
            !metadata.file_type().is_symlink(),
            "benchmark must not follow external paths"
        );
        total += metadata.blocks() * 512;
        if metadata.is_dir() {
            total += disk_bytes(&entry.path())?;
        }
    }
    Ok(total)
}

struct Monitor {
    stop: Arc<AtomicBool>,
    phase: Arc<AtomicU64>,
    peak: Arc<AtomicU64>,
    mixed_peak: Arc<AtomicU64>,
    disk_peak: Arc<AtomicU64>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Monitor {
    fn start(root: PathBuf) -> Self {
        let mut monitor = Self {
            stop: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(AtomicU64::new(0)),
            peak: Arc::new(AtomicU64::new(0)),
            mixed_peak: Arc::new(AtomicU64::new(0)),
            disk_peak: Arc::new(AtomicU64::new(0)),
            error: Arc::new(std::sync::Mutex::new(None)),
            thread: None,
        };
        let (stop, phase, peak, mixed_peak, disk_peak, error) = (
            monitor.stop.clone(),
            monitor.phase.clone(),
            monitor.peak.clone(),
            monitor.mixed_peak.clone(),
            monitor.disk_peak.clone(),
            monitor.error.clone(),
        );
        monitor.thread = Some(std::thread::spawn(move || {
            let mut tick = 0;
            while !stop.load(Ordering::Relaxed) {
                let rss = rss_tree(std::process::id(), &mut HashSet::new());
                peak.fetch_max(rss, Ordering::Relaxed);
                if phase.load(Ordering::Relaxed) == 1 {
                    mixed_peak.fetch_max(rss, Ordering::Relaxed);
                }
                let failure = if rss > RSS_CAP {
                    Some("self + child RSS exceeded 2 GiB".to_owned())
                } else if tick % 20 == 0 {
                    match disk_bytes(&root) {
                        Ok(bytes) => {
                            disk_peak.fetch_max(bytes, Ordering::Relaxed);
                            (bytes >= DISK_CAP)
                                .then(|| "owned disk footprint exceeded 16 GB".to_owned())
                        }
                        Err(failure) => Some(format!("resource sampling failed: {failure}")),
                    }
                } else {
                    None
                };
                if let Some(failure) = failure {
                    *error.lock().unwrap() = Some(failure);
                    break;
                }
                tick += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
        }));
        monitor
    }

    fn check(&self) -> Result<()> {
        if let Some(error) = self.error.lock().unwrap().as_ref() {
            return Err(error.clone().into());
        }
        Ok(())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("resource sampler failed");
        }
    }
}

async fn run(directory: &Path, count: usize, monitor: &Monitor) -> Result<Value> {
    let root_path = directory.join("library");
    fs::create_dir(&root_path)?;
    let start = Instant::now();
    let corpus = owned_corpus(&root_path, count)?;
    let generation_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!("BENCHMARK_CORPUS {}", corpus);
    let state = directory.join("state");
    let store = SqliteStore::open(&state).await?;
    // Bound the only growing data store as well as the generated corpus. WAL/SHM
    // also inherit the per-file kernel limit installed by the test entrypoint.
    let mut tx = store.begin_write().await?;
    let pages: i64 = sqlx::query_scalar("PRAGMA max_page_count = 65536")
        .fetch_one(&mut *tx)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *tx)
        .await?;
    assert!(pages * page_size <= 256 * 1024 * 1024);
    tx.commit().await?;
    let settings = Settings::new(store.clone(), EncryptionKey::load_or_create(&state).await?);
    let auth = AuthService::new(store.clone());
    auth.setup("benchmark-owner", "owned benchmark password only")
        .await?;
    let admin = auth
        .create_key("benchmark-admin", Scope::Admin)
        .await?
        .secret;
    let mut tokens = Vec::new();
    for index in 0..5 {
        tokens.push(
            auth.create_key(&format!("reader-{index}"), Scope::Read)
                .await?
                .secret,
        );
    }
    let router = app::router_with_settings(store.clone(), ORIGIN, settings.clone());
    let library = Library::new(store.clone());
    let root = library.register_root("Owned benchmark", &root_path).await?;
    let idle_rss = rss_tree(std::process::id(), &mut HashSet::new());
    let initial = scan(&library, &store, &root.id, count).await?;
    assert_eq!(initial["reason_counts"]["unmatched"], count);
    let start = Instant::now();
    seed(&store, &root_path, &root.id, count).await?;
    let seed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let incremental = scan(&library, &store, &root.id, count).await?;
    assert_eq!(incremental["reason_counts"]["metadata_unchanged"], count);
    println!(
        "BENCHMARK_SCANS {}",
        json!({"initial":initial,"incremental":incremental})
    );

    let mut reader_times = Timings::new();
    for format in ["cbz", "pdf"] {
        let index = if format == "cbz" { 0 } else { 8 };
        for _ in 0..10 {
            let reader = ReaderService::new(store.clone());
            for temperature in ["cold", "warm"] {
                let start = Instant::now();
                let page = reader.page(&format!("bench-{index:05}"), 0).await?;
                assert!(!page.bytes.is_empty());
                record(
                    &mut reader_times,
                    &format!("{format}_{temperature}_page"),
                    start,
                );
            }
        }
    }
    // Traverse once to warm local DB/OS caches, then measure five independent cursor chains.
    session(&store, &router, &tokens[0], count, 0, None).await?;
    let warm = five_sessions(&store, &router, &tokens, count, false).await?;
    monitor.phase.store(1, Ordering::Relaxed);
    let active = AtomicBool::new(false);
    let ready = tokio::sync::Notify::new();
    let finish = AtomicBool::new(false);
    let repeated_scans = async {
        let mut reports = Vec::new();
        loop {
            active.store(true, Ordering::SeqCst);
            let pending = scan(&library, &store, &root.id, count);
            tokio::pin!(pending);
            // Poll scan before releasing the load so it has started real I/O.
            tokio::select! {
                result = &mut pending => { reports.push(result?); }
                _ = tokio::task::yield_now() => {
                    ready.notify_one();
                    reports.push(pending.await?);
                }
            }
            active.store(false, Ordering::SeqCst);
            if finish.load(Ordering::SeqCst) {
                break;
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(reports)
    };
    let load = async {
        ready.notified().await;
        assert!(active.load(Ordering::SeqCst));
        let result = tokio::try_join!(
            five_sessions(&store, &router, &tokens, count, true),
            writes(&store, &router, &admin, &settings, &root.id, count)
        );
        finish.store(true, Ordering::SeqCst);
        result
    };
    let (mixed_scans, (mixed_readers, mixed_writes)) = tokio::try_join!(repeated_scans, load)?;
    monitor.phase.store(0, Ordering::Relaxed);
    for query in [
        "SELECT COUNT(*) FROM library_files",
        "SELECT COUNT(*) FROM file_coverage",
    ] {
        let actual: i64 = sqlx::query_scalar(query).fetch_one(store.reader()).await?;
        assert_eq!(actual, count as i64, "{query}");
    }
    let disk = disk_bytes(directory)?;
    monitor.disk_peak.fetch_max(disk, Ordering::Relaxed);
    assert!(disk < DISK_CAP);
    monitor.check()?;
    store.close().await;
    Ok(
        json!({"requested_files":count,"target_files":TARGET,"full_50k":count==TARGET,
        "profile":if cfg!(debug_assertions) {"debug"} else {"release"}, "embedded_ui":cfg!(feature="embedded-ui"),
        "corpus":corpus,"generation_ms":generation_ms,"seed_ms":seed_ms,"seed_batch_size":BATCH,
        "catalog_rows_each":count,"final_file_coverage_rows":count,"initial_scan":initial,"incremental_scan":incremental,
        "five_session_warm":summary(warm),"reader_cold_warm":summary(reader_times),
        "five_session_mixed":summary(mixed_readers),"mixed_writes":summary(mixed_writes),"mixed_scans":mixed_scans,
        "rss_bytes":{"idle":idle_rss,"peak_self_plus_children_sampled":monitor.peak.load(Ordering::Relaxed),
            "mixed_peak_sampled":monitor.mixed_peak.load(Ordering::Relaxed),"hard_stop_limit":RSS_CAP,
            "idle_250mib_met":idle_rss < 250*1024*1024,
            "mixed_1gib_met":monitor.mixed_peak.load(Ordering::Relaxed) < 1024*1024*1024},
        "peak_owned_disk_bytes":monitor.disk_peak.load(Ordering::Relaxed),"disk_cap_bytes":DISK_CAP,
        "exclusions":["No NAS or real-library access; only two owned tiny synthetic fixtures",
            "In-process HTTP router including authentication/body collection; no network, LAN, TLS or browser timing",
            "Cold means fresh ReaderService manifest/page caches; warm repeats use its page cache; OS cache is not dropped",
            "Imports measure preview/accept association, not copy/move acquisition pipeline",
            "RSS includes test harness and sampled descendants; sub-50ms child peaks can be missed",
            "50ms RSS and 1s disk watchdogs are sampled guardrails, not kernel aggregate quotas",
            "Build artifact disk/RSS is excluded; UI build remains owned by the root task"]}),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "creates 50,000 owned regular files; run scripts/benchmark explicitly"]
async fn owned_50000_file_performance() -> Result<()> {
    let count =
        std::env::var("BENCHMARK_FILES").map_or(Ok(TARGET), |value| value.parse::<usize>())?;
    assert!(
        (1000..=TARGET).contains(&count),
        "BENCHMARK_FILES must be 1000..=50000; no silent cap"
    );
    assert!(
        Path::new("/proc/self/status").is_file(),
        "Linux /proc is required for resource accounting"
    );
    assert!(rss_tree(std::process::id(), &mut HashSet::new()) > 0);
    let limited = std::process::Command::new("prlimit")
        .args([
            "--pid",
            &std::process::id().to_string(),
            "--fsize=536870912:536870912",
        ])
        .status()?;
    assert!(
        limited.success(),
        "cannot install benchmark file-size hard limit"
    );
    // Always stay under the checkout, even if TMPDIR points to real storage.
    let directory = tempfile::Builder::new()
        .prefix(".benchmark-owned-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let monitor = Monitor::start(directory.path().to_owned());
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            monitor.check()?;
        }
        #[allow(unreachable_code)]
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(3600), run(directory.path(), count, &monitor)) => result?,
        failure = watchdog => { failure?; unreachable!() },
    };
    drop(monitor);
    directory.close()?;
    let report = result?;
    println!("BENCHMARK_RESULT {}", serde_json::to_string(&report)?);
    Ok(())
}

// Separate opt-in: bounded socket HTTP and Copy journals, not the full benchmark rerun.
type HttpWindows = Vec<(&'static str, f64, f64)>;

async fn loopback_get(client: &reqwest::Client, url: &str, token: &str) -> Result<Vec<u8>> {
    let mut response = client.get(url).bearer_auth(token).send().await?;
    if response.status() != StatusCode::OK {
        return Err(format!("loopback HTTP status {}", response.status()).into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err("loopback response exceeded 2 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err("empty loopback response".into());
    }
    Ok(bytes)
}

async fn bounded_http_session(
    client: &reqwest::Client,
    origin: &str,
    token: &str,
    clock: Instant,
) -> Result<(Timings, HttpWindows)> {
    let mut times = Timings::new();
    let mut windows = Vec::with_capacity(40);
    let mut cursor = None::<String>;
    for index in 0..20 {
        let mut url = reqwest::Url::parse(&format!("{origin}/api/v1/publications"))?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = &cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        let start = Instant::now();
        let bytes = loopback_get(client, url.as_str(), token).await?;
        let end = Instant::now();
        record(&mut times, "catalog_http", start);
        windows.push((
            "catalog_http",
            start.duration_since(clock).as_secs_f64(),
            end.duration_since(clock).as_secs_f64(),
        ));
        let page: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(page["items"].as_array().map(Vec::len), Some(100));
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        assert!(cursor.is_some());
        let file = if index % 2 == 0 {
            "bench-00000"
        } else {
            "bench-00008"
        };
        let start = Instant::now();
        loopback_get(
            client,
            &format!("{origin}/api/v1/library/files/{file}/pages/0"),
            token,
        )
        .await?;
        let end = Instant::now();
        record(&mut times, "warm_reader_http", start);
        windows.push((
            "warm_reader_http",
            start.duration_since(clock).as_secs_f64(),
            end.duration_since(clock).as_secs_f64(),
        ));
    }
    Ok((times, windows))
}

async fn journal_http_run(directory: &Path, monitor: &Monitor) -> Result<Value> {
    use libraryd::importer::journal::{
        ImportPhase, ImportPolicy, ImportService, InternalImportRequest,
    };

    let root_path = directory.join("library");
    let destination_path = directory.join("copies");
    fs::create_dir(&root_path)?;
    fs::create_dir(&destination_path)?;
    let corpus = owned_corpus(&root_path, TARGET)?;
    monitor.check()?;
    let state = directory.join("state");
    let store = SqliteStore::open(&state).await?;
    let mut tx = store.begin_write().await?;
    let pages: i64 = sqlx::query_scalar("PRAGMA max_page_count = 65536")
        .fetch_one(&mut *tx)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *tx)
        .await?;
    assert!(pages * page_size <= 256 * 1024 * 1024);
    tx.commit().await?;
    let library = Library::new(store.clone());
    let root = library
        .register_root("Owned targeted corpus", &root_path)
        .await?;
    let destination = library
        .register_root("Owned targeted copies", &destination_path)
        .await?;
    scan(&library, &store, &root.id, TARGET).await?;
    seed(&store, &root_path, &root.id, TARGET).await?;
    let settings = Settings::new(store.clone(), EncryptionKey::load_or_create(&state).await?);
    let auth = AuthService::new(store.clone());
    auth.setup("targeted-owner", "owned targeted benchmark password")
        .await?;
    let mut tokens = Vec::new();
    let mut clients = Vec::new();
    for index in 0..5 {
        tokens.push(
            auth.create_key(&format!("targeted-reader-{index}"), Scope::Read)
                .await?
                .secret,
        );
        clients.push(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .pool_max_idle_per_host(1)
                .build()?,
        );
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let router = app::router_with_settings(store.clone(), &origin, settings);
    // JoinSet aborts the listener on errors; the dedicated runtime is dropped before temp cleanup.
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, router).await });
    for (client, token) in clients.iter().zip(&tokens) {
        for path in [
            "/api/v1/publications?limit=100",
            "/api/v1/library/files/bench-00000/pages/0",
            "/api/v1/library/files/bench-00008/pages/0",
        ] {
            loopback_get(client, &format!("{origin}{path}"), token).await?;
        }
    }
    let idle_rss = rss_tree(std::process::id(), &mut HashSet::new());
    monitor.phase.store(1, Ordering::Relaxed);
    let clock = Instant::now();
    let scan_finished = AtomicBool::new(false);
    let scanning = async {
        let start = clock.elapsed().as_secs_f64();
        let result = library.scan(&root.id).await;
        let end = clock.elapsed().as_secs_f64();
        scan_finished.store(true, Ordering::SeqCst);
        (start, end, result)
    };
    let copying = async {
        let service = ImportService::new(store.clone());
        let mut copies = Vec::new();
        // Both units are among seed()'s unassociated RESERVED entries.
        for index in [TARGET - RESERVED, TARGET - RESERVED + 8] {
            let name = relative(index);
            let output = if name.ends_with(".pdf") {
                "copied.pdf"
            } else {
                "copied.cbz"
            };
            let source = root_path.join(&name);
            let before = fs::metadata(&source)?;
            let original = fs::read(&source)?;
            let id = uuid::Uuid::new_v4().to_string();
            let start = clock.elapsed().as_secs_f64();
            service
                .plan(
                    &id,
                    InternalImportRequest {
                        source_root: root.id.clone(),
                        source_relative: name,
                        destination_root: destination.id.clone(),
                        destination_relative: output.into(),
                        unit_id: format!("bench-{index:05}"),
                        policy: ImportPolicy::Copy,
                    },
                )
                .await?;
            let completed = service.recover(&id).await?;
            let end = clock.elapsed().as_secs_f64();
            assert_eq!(completed.phase, ImportPhase::Done);
            assert_eq!(completed.effective_policy.as_deref(), Some("copy"));
            assert!(completed.library_file_id.is_some());
            let after = fs::metadata(&source)?;
            let copied = fs::metadata(destination_path.join(output))?;
            assert_eq!(
                (
                    before.dev(),
                    before.ino(),
                    before.len(),
                    before.mtime(),
                    before.mtime_nsec()
                ),
                (
                    after.dev(),
                    after.ino(),
                    after.len(),
                    after.mtime(),
                    after.mtime_nsec()
                )
            );
            assert_ne!((before.dev(), before.ino()), (copied.dev(), copied.ino()));
            assert_eq!(fs::read(&source)?, original);
            assert_eq!(fs::read(destination_path.join(output))?, original);
            copies.push(
                json!({"format":if output.ends_with("pdf") {"pdf"} else {"cbz"},
                "start_seconds":start,"end_seconds":end,"elapsed_ms":(end-start)*1000.0,
                "bytes":original.len(),"phase":"done","effective_policy":"copy"}),
            );
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(copies)
    };
    let load = async {
        let observed_run = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                // visited is only written at completion. A committed entry for a running
                // run proves inventory progress without adding production instrumentation.
                let run: Option<String> = sqlx::query_scalar(
                    "SELECT r.id FROM scan_runs r WHERE r.root_id=? AND r.state='running' AND EXISTS (SELECT 1 FROM scan_entries e WHERE e.root_id=r.root_id AND e.last_seen_run_id=r.id) LIMIT 1",
                ).bind(&root.id).fetch_optional(store.reader()).await?;
                if scan_finished.load(Ordering::SeqCst) {
                    return Err("scan finished before persisted progress was observed".into());
                }
                if let Some(run) = run {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(run);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let progress_observed_at = clock.elapsed().as_secs_f64();
        // join! drains all bounded operations even when one fails; no detached copy on ordinary errors.
        let results = tokio::join!(
            copying,
            bounded_http_session(&clients[0], &origin, &tokens[0], clock),
            bounded_http_session(&clients[1], &origin, &tokens[1], clock),
            bounded_http_session(&clients[2], &origin, &tokens[2], clock),
            bounded_http_session(&clients[3], &origin, &tokens[3], clock),
            bounded_http_session(&clients[4], &origin, &tokens[4], clock),
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            observed_run,
            progress_observed_at,
            results,
        ))
    };
    let ((scan_start, scan_end, scan_result), load_result) = tokio::join!(scanning, load);
    monitor.phase.store(0, Ordering::Relaxed);
    server.shutdown().await;
    let report = scan_result?;
    assert_eq!(report.visited, TARGET as u64);
    assert_eq!((report.errors, report.skipped), (0, 0));
    let (observed_run, progress_observed_at, (copies, a, b, c, d, e)) = load_result?;
    assert_eq!(observed_run, report.run_id);
    // Reason aggregation is outside the recorded Library::scan interval.
    let rows = sqlx::query("SELECT COALESCE(reason,'none') AS reason, COUNT(*) AS count FROM scan_entries WHERE last_seen_run_id=? GROUP BY reason")
        .bind(&report.run_id).fetch_all(store.reader()).await?;
    let reasons: BTreeMap<String, i64> = rows
        .iter()
        .map(|row| (row.get("reason"), row.get("count")))
        .collect();
    assert_eq!(reasons.get("metadata_unchanged"), Some(&(TARGET as i64)));
    let scan_result = json!({"elapsed_ms":(scan_end-scan_start)*1000.0,
        "report":report,"reason_counts":reasons});
    let mut copies = copies?;
    let mut overlap_ok = true;
    for copy in &mut copies {
        let overlap = (copy["end_seconds"].as_f64().unwrap().min(scan_end)
            - copy["start_seconds"].as_f64().unwrap().max(scan_start))
        .max(0.0);
        copy["scan_overlap_ms"] = json!(overlap * 1000.0);
        overlap_ok &= overlap > 0.0;
    }
    let mut times = Timings::new();
    let mut overlaps = Vec::new();
    for session in [a, b, c, d, e] {
        let (session_times, windows) = session?;
        for (name, values) in session_times {
            times.entry(name).or_default().extend(values);
        }
        let catalog = windows
            .iter()
            .filter(|(name, start, end)| {
                *name == "catalog_http" && *start < scan_end && *end > scan_start
            })
            .count();
        let reader = windows
            .iter()
            .filter(|(name, start, end)| {
                *name == "warm_reader_http" && *start < scan_end && *end > scan_start
            })
            .count();
        overlap_ok &= catalog > 0 && reader > 0;
        overlaps.push(json!({"catalog_requests_overlapping_scan":catalog,"reader_requests_overlapping_scan":reader}));
    }
    for query in [
        "SELECT COUNT(*) FROM library_files",
        "SELECT COUNT(*) FROM file_coverage",
    ] {
        let actual: i64 = sqlx::query_scalar(query).fetch_one(store.reader()).await?;
        assert_eq!(actual, (TARGET - RESERVED + 2) as i64);
    }
    let journals: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM import_operations WHERE phase='done'")
            .fetch_one(store.reader())
            .await?;
    assert_eq!(journals, 2);
    monitor.check()?;
    store.close().await;
    Ok(
        json!({"corpus":corpus,"catalog_rows":TARGET,"cataloged_files":TARGET-RESERVED+2,
        "initial_scan":"setup only; excluded from targeted timings", "unchanged_scan":scan_result,
        "scan_window_seconds":[scan_start,scan_end],"progress_observed_at_seconds":progress_observed_at,
        "progress_observed_run_id":observed_run,"overlap_verified":overlap_ok,
        "http":summary(times),"session_overlap":overlaps,"serial_copy_journals":copies,
        "idle_rss_bytes":idle_rss,"idle_250mib_met":idle_rss < 250*1024*1024,
        "targets":{"catalog_p95_ms":300,"warm_reader_p95_ms":1000},
        "exclusions":["Tiny synthetic fixtures; no large-file throughput or move measurement",
            "Real ephemeral loopback HTTP sockets; not the deployed artifact, LAN, TLS, or browser latency",
            "Warm-up excluded; OS caches not dropped; 100 catalog and 100 reader requests total",
            "Two serial Copy journals are individual durations, not throughput percentiles",
            "Root must enforce a shared one-CPU scope including runtime, sampler, server, and decoder children"]}),
    )
}

#[test]
#[ignore = "requires BENCHMARK_JOURNAL_HTTP=OWNED_50K_ONE_CPU and root-owned one-CPU scope"]
fn owned_50000_journal_copy_loopback_http() -> Result<()> {
    if std::env::var("BENCHMARK_JOURNAL_HTTP").as_deref() != Ok("OWNED_50K_ONE_CPU") {
        return Err(
            "set BENCHMARK_JOURNAL_HTTP=OWNED_50K_ONE_CPU inside a shared one-CPU scope".into(),
        );
    }
    let limited = std::process::Command::new("prlimit")
        .args([
            "--pid",
            &std::process::id().to_string(),
            "--fsize=536870912:536870912",
        ])
        .status()?;
    if !limited.success() {
        return Err("cannot install per-file limit".into());
    }
    let directory = tempfile::Builder::new()
        .prefix(".benchmark-journal-http-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    fs::set_permissions(
        directory.path(),
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )?;
    let mut monitor = Monitor::start(directory.path().to_owned());
    // Dropping this dedicated runtime waits for blocking work before deleting owned files.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Value> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let watchdog = async {
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    monitor.check()?;
                }
                #[allow(unreachable_code)]
                Ok::<Value, Box<dyn std::error::Error + Send + Sync>>(Value::Null)
            };
            tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(300), journal_http_run(directory.path(), &monitor)) => result?,
                failure = watchdog => failure,
            }
        })
    }));
    monitor.stop.store(true, Ordering::SeqCst);
    let monitor_result = if monitor
        .thread
        .take()
        .is_some_and(|thread| thread.join().is_err())
    {
        Err("resource sampler panicked".into())
    } else {
        monitor.check()
    };
    // The sampler has stopped and joined; its final metrics and violation are now stable.
    let peak = monitor.peak.load(Ordering::Relaxed);
    let mixed_peak = monitor.mixed_peak.load(Ordering::Relaxed);
    let disk_peak = monitor.disk_peak.load(Ordering::Relaxed);
    drop(monitor);
    let cleanup = directory.close();
    let result = match outcome {
        Ok(result) => result,
        Err(_) => Err("targeted benchmark assertion panicked".into()),
    };
    let mut failures = Vec::new();
    if let Err(error) = &result {
        failures.push(error.to_string().chars().take(512).collect::<String>());
    }
    if let Err(error) = monitor_result {
        failures.push(error.to_string().chars().take(512).collect());
    }
    if let Err(error) = &cleanup {
        failures.push(error.to_string().chars().take(512).collect());
    }
    if result
        .as_ref()
        .is_ok_and(|report| report["overlap_verified"] != true)
    {
        failures.push(
            "scan overlap was not observed for both copies and all five HTTP sessions".into(),
        );
    }
    let passed = failures.is_empty();
    println!(
        "BENCHMARK_JOURNAL_HTTP_RESULT {}",
        json!({"passed":passed,
        "profile":if cfg!(debug_assertions) {"debug"} else {"release"},
        "failures":failures,"measurements":result.ok(),"cleanup_completed":cleanup.is_ok(),
        "rss_bytes":{"peak_self_plus_children_sampled":peak,"mixed_peak_sampled":mixed_peak,
            "mixed_1gib_met":mixed_peak < 1024*1024*1024,"hard_stop_limit":RSS_CAP},
        "peak_owned_disk_bytes":disk_peak,"disk_cap_bytes":DISK_CAP,
        "guardrails":"300s async deadline; 15s HTTP deadline; 2MiB HTTP bodies; 512MiB per-file limit; sampled RSS/disk limits may overshoot"})
    );
    if !passed {
        return Err("targeted journal/HTTP check failed; see bounded JSON report".into());
    }
    Ok(())
}
