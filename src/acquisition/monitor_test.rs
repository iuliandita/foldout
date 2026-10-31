use super::*;
use crate::{
    search::mock,
    settings::{CreateIntegration, EncryptionKey, SecretUpdate},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const UNIT: &str = "11111111-1111-1111-1111-111111111111";
const UNIT2: &str = "22222222-2222-2222-2222-222222222222";

struct Fixture {
    dir: tempfile::TempDir,
    store: SqliteStore,
    settings: Settings,
    monitor: Monitor,
    source: String,
    mode: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let dir = mock::private_directory();
        let store = SqliteStore::open(dir.path()).await.unwrap();
        let settings = Settings::new(
            store.clone(),
            EncryptionKey::load_or_create(dir.path()).await.unwrap(),
        );
        let mode = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let state = (mode.clone(), calls.clone());
        let router = axum::Router::new().route("/7/api", axum::routing::get(
            |axum::extract::State((mode, calls)): axum::extract::State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
                if q.get("t").is_some_and(|t| t == "caps") {
                    return (axum::http::StatusCode::OK, "<caps><limits max=\"100\"/><searching><search available=\"yes\" supportedParams=\"q\"/></searching><categories><category id=\"7030\"/></categories></caps>".to_string());
                }
                calls.fetch_add(1, Ordering::SeqCst);
                while mode.load(Ordering::SeqCst) == 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                if mode.load(Ordering::SeqCst) == 1 { return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "private failure".into()); }
                let item = if mode.load(Ordering::SeqCst) == 2 { "<item><title>Ambiguous issue</title><guid>https://private.invalid/token=secret</guid><enclosure url=\"http://localhost/private?secret=hidden\" length=\"512\" type=\"application/x-nzb\"/><newznab:attr name=\"category\" value=\"7030\"/></item>" } else { "" };
                (axum::http::StatusCode::OK, format!("<rss xmlns:newznab=\"http://www.newznab.com/DTD/2010/feeds/attributes/\"><channel>{item}</channel></rss>"))
            }
        )).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let source = settings.create(CreateIntegration {
            kind: IntegrationKind::Prowlarr, label: "Monitor fixture".into(), base_url: url, enabled: true,
            options: serde_json::json!({"indexer_id":7,"protocol":"usenet","categories":{"comics":[7030]}}),
            api_key: SecretUpdate::Set("private-fixture-key".into()), username: SecretUpdate::Preserve, password: SecretUpdate::Preserve,
        }).await.unwrap().id;
        let mut tx = store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO publications(id,content_type,title,sort_title) VALUES ('p','comic','Fixture','fixture')").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO editions(id,publication_id,language) VALUES ('e','p','en')")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO units(id,edition_id,label,kind) VALUES (?,'e','12.5','issue')")
            .bind(UNIT)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let monitor = Monitor::new(store.clone(), settings.clone());
        Self {
            dir,
            store,
            settings,
            monitor,
            source,
            mode,
            calls,
            server,
        }
    }
    fn request(&self) -> CreateMonitor {
        CreateMonitor {
            unit_id: UNIT.into(),
            integration_id: self.source.clone(),
            query: "Fixture 12.5".into(),
            interval_seconds: 900,
            schedule: MonitorSchedule::Interval,
            timezone: "UTC".into(),
            enabled: true,
            selection_policy: SelectionPolicy::ReviewOnly,
        }
    }
    async fn due(&self, item: &MonitorView) -> MonitorView {
        mock::allow_search(&self.store).await;
        self.monitor
            .run("owner", &item.id, item.revision)
            .await
            .unwrap()
    }
}

fn timestamp(value: &str) -> i64 {
    value.parse::<jiff::Timestamp>().unwrap().as_second()
}

#[test]
fn calendar_boundaries_handle_dst_and_local_dates_without_duplicate_folds() {
    for (zone, time, weekday, after, expected) in [
        (
            "Europe/Berlin",
            "02:30",
            None,
            "2026-03-29T00:00:00Z",
            "2026-03-29T01:00:00Z",
        ),
        (
            "Europe/Berlin",
            "02:30",
            Some(7),
            "2026-03-29T00:00:00Z",
            "2026-03-29T01:00:00Z",
        ),
        (
            "Europe/Berlin",
            "02:30",
            None,
            "2026-03-29T01:00:00Z",
            "2026-03-30T00:30:00Z",
        ),
        (
            "Europe/Berlin",
            "02:30",
            None,
            "2026-10-25T00:00:00Z",
            "2026-10-25T00:30:00Z",
        ),
        (
            "Europe/Berlin",
            "02:30",
            None,
            "2026-10-25T00:30:00Z",
            "2026-10-26T01:30:00Z",
        ),
        (
            "Europe/Berlin",
            "02:30",
            Some(7),
            "2026-10-25T00:31:00Z",
            "2026-11-01T01:30:00Z",
        ),
        (
            "America/New_York",
            "02:30",
            None,
            "2026-03-08T05:00:00Z",
            "2026-03-08T07:00:00Z",
        ),
        (
            "America/New_York",
            "01:30",
            None,
            "2026-11-01T04:00:00Z",
            "2026-11-01T05:30:00Z",
        ),
        (
            "Australia/Lord_Howe",
            "02:15",
            None,
            "2026-10-03T14:00:00Z",
            "2026-10-03T15:30:00Z",
        ),
        (
            "Australia/Lord_Howe",
            "01:45",
            None,
            "2026-04-04T13:00:00Z",
            "2026-04-04T14:45:00Z",
        ),
        (
            "Asia/Kathmandu",
            "00:15",
            None,
            "2026-01-01T18:00:00Z",
            "2026-01-01T18:30:00Z",
        ),
        (
            "UTC",
            "00:00",
            Some(1),
            "2026-12-31T23:59:59Z",
            "2027-01-04T00:00:00Z",
        ),
        (
            "Pacific/Apia",
            "12:00",
            None,
            "2011-12-30T00:00:00Z",
            "2011-12-30T22:00:00Z",
        ),
        (
            "Pacific/Apia",
            "12:00",
            Some(5),
            "2011-12-30T00:00:00Z",
            "2012-01-05T22:00:00Z",
        ),
    ] {
        let schedule = match weekday {
            Some(weekday) => MonitorSchedule::Weekly {
                time: time.into(),
                weekday,
            },
            None => MonitorSchedule::Daily { time: time.into() },
        };
        assert_eq!(
            schedule.next_run(timestamp(after), 900, zone).unwrap(),
            timestamp(expected),
            "{schedule:?} {zone} after {after}"
        );
    }
    for (after, interval, expected) in [
        (0, 900, 900),
        (899, 900, 900),
        (900, 900, 1800),
        (1000, 3600, 3600),
    ] {
        assert_eq!(
            MonitorSchedule::Interval
                .next_run(after, interval, "UTC")
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn invalid_calendar_inputs_do_not_mutate_the_monitor() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    for (schedule, zone) in [
        (
            MonitorSchedule::Daily {
                time: "24:00".into(),
            },
            "UTC",
        ),
        (
            MonitorSchedule::Daily {
                time: "09:60".into(),
            },
            "UTC",
        ),
        (
            MonitorSchedule::Daily {
                time: "9:00".into(),
            },
            "UTC",
        ),
        (
            MonitorSchedule::Daily {
                time: "09:00:00".into(),
            },
            "UTC",
        ),
        (
            MonitorSchedule::Daily {
                time: "09:00".into(),
            },
            "Invalid/Zone",
        ),
        (
            MonitorSchedule::Daily {
                time: "09:00".into(),
            },
            "+02:00",
        ),
        (MonitorSchedule::Interval, "Europe/Berlin"),
        (
            MonitorSchedule::Weekly {
                time: "09:00".into(),
                weekday: 0,
            },
            "UTC",
        ),
        (
            MonitorSchedule::Weekly {
                time: "09:00".into(),
                weekday: 8,
            },
            "UTC",
        ),
    ] {
        assert!(
            matches!(
                f.monitor
                    .update(
                        "owner",
                        &item.id,
                        UpdateMonitor {
                            revision: item.revision,
                            schedule: Some(schedule.clone()),
                            timezone: Some(zone.into()),
                            ..Default::default()
                        }
                    )
                    .await,
                Err(MonitorError::Invalid)
            ),
            "{schedule:?} {zone}"
        );
        let mut request = f.request();
        request.schedule = schedule;
        request.timezone = zone.into();
        assert!(matches!(
            f.monitor.create("other", request).await,
            Err(MonitorError::Invalid)
        ));
    }
    for json in [
        r#"{"revision":1,"schedule":null}"#,
        r#"{"revision":1,"timezone":null}"#,
        r#"{"revision":1,"schedule":{"kind":"cron","time":"09:00"}}"#,
        r#"{"revision":1,"schedule":{"kind":"daily","time":"09:00","weekday":1}}"#,
        r#"{"revision":1,"schedule":{"kind":"weekly","time":"09:00"}}"#,
    ] {
        assert!(
            serde_json::from_str::<UpdateMonitor>(json).is_err(),
            "{json}"
        );
    }
    let current = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(current.revision, item.revision);
    assert_eq!(current.next_run, item.next_run);
    assert_eq!(current.schedule, MonitorSchedule::Interval);
    assert_eq!(current.timezone, "UTC");
}

#[tokio::test]
async fn calendar_restart_coalesces_missed_runs_and_schedule_edits_keep_leases() {
    for schedule in [
        MonitorSchedule::Daily {
            time: "02:30".into(),
        },
        MonitorSchedule::Weekly {
            time: "02:30".into(),
            weekday: 7,
        },
    ] {
        let f = Fixture::new().await;
        let mut request = f.request();
        request.schedule = schedule.clone();
        request.timezone = "Europe/Berlin".into();
        let item = f.monitor.create("owner", request).await.unwrap();
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE monitors SET next_run = unixepoch() - 2592000 WHERE id = ?")
            .bind(&item.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        mock::allow_search(&f.store).await;
        let worker = Monitor::new(
            SqliteStore::open(f.dir.path()).await.unwrap(),
            f.settings.clone(),
        );
        assert!(worker.tick().await.unwrap());
        assert!(!worker.tick().await.unwrap());
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        let current = worker.get("owner", &item.id).await.unwrap();
        assert_eq!(current.schedule, schedule);
        assert_eq!(current.timezone, "Europe/Berlin");
        assert!(current.next_run > now());
        let due = f.due(&current).await;
        let claim = worker.claim().await.unwrap().unwrap();
        let edited = worker
            .update(
                "owner",
                &item.id,
                UpdateMonitor {
                    revision: due.revision,
                    schedule: Some(MonitorSchedule::Weekly {
                        time: "11:15".into(),
                        weekday: 1,
                    }),
                    timezone: Some("America/New_York".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(edited.running);
        assert!(matches!(
            worker
                .update(
                    "owner",
                    &item.id,
                    UpdateMonitor {
                        revision: due.revision,
                        timezone: Some("UTC".into()),
                        ..Default::default()
                    }
                )
                .await,
            Err(MonitorError::Conflict)
        ));
        f.due(&edited).await;
        assert!(worker.claim().await.unwrap().is_none());
        let result = Ok(Releases {
            releases: vec![],
            offset: 0,
            total: None,
            next_offset: None,
            target: None,
        });
        assert!(!worker.finish(&claim, result).await.unwrap());
        assert!(worker.claim().await.unwrap().is_some());
        let current = worker.get("owner", &item.id).await.unwrap();
        assert_eq!(current.timezone, "America/New_York");
        assert!(current.candidates.is_empty());
    }
}

#[tokio::test]
async fn calendar_migration_preserves_legacy_rows_and_rejects_inconsistent_schedules() {
    use sqlx::Connection;
    let mut db = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    migrations.run_to(18, &mut db).await.unwrap();
    for statement in [
        "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p','comic','Fixture','fixture')",
        "INSERT INTO editions(id,publication_id,language) VALUES('e','p','en')",
        "INSERT INTO units(id,edition_id,label,kind) VALUES('u','e','1','issue')",
        "INSERT INTO integrations(id,kind,label,base_url,enabled,options,secret_version,secret_nonce,secret_ciphertext) VALUES('i','prowlarr','Fixture','http://localhost',1,'{}',1,zeroblob(24),zeroblob(28))",
        "INSERT INTO monitors(id,owner,unit_id,integration_id,content_type,query,interval_seconds,selection_policy,enabled,next_run,last_state,lease_token,lease_until) VALUES('m','owner','u','i','comic','q',900,'review_only',1,123,'scheduled','lease',456)",
    ] {
        sqlx::query(statement).execute(&mut db).await.unwrap();
    }
    migrations.run(&mut db).await.unwrap();
    let row = sqlx::query("SELECT * FROM monitors WHERE id = 'm'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(
        MonitorSchedule::decode(&row).unwrap(),
        MonitorSchedule::Interval
    );
    assert_eq!(row.get::<String, _>("timezone"), "UTC");
    assert_eq!(row.get::<i64, _>("next_run"), 123);
    assert_eq!(row.get::<i64, _>("revision"), 1);
    assert_eq!(row.get::<String, _>("lease_token"), "lease");
    assert_eq!(row.get::<i64, _>("lease_until"), 456);
    for statement in [
        "UPDATE monitors SET schedule_kind = 'cron' WHERE id = 'm'",
        "UPDATE monitors SET timezone = '' WHERE id = 'm'",
        "UPDATE monitors SET timezone = 'Europe/Berlin' WHERE id = 'm'",
        "UPDATE monitors SET schedule_time = '09:00' WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'daily' WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'daily', schedule_time = '24:00' WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'daily', schedule_time = '09:00', schedule_weekday = 1 WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'weekly', schedule_time = '09:00' WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'weekly', schedule_time = '09:00', schedule_weekday = 8 WHERE id = 'm'",
        "UPDATE monitors SET schedule_kind = 'weekly', schedule_time = '09:00', schedule_weekday = 1.5 WHERE id = 'm'",
    ] {
        assert!(
            sqlx::query(statement).execute(&mut db).await.is_err(),
            "{statement}"
        );
    }
    sqlx::query("UPDATE monitors SET schedule_kind = 'weekly', schedule_time = '09:00', schedule_weekday = 1, timezone = 'Europe/Berlin' WHERE id = 'm'")
        .execute(&mut db).await.unwrap();
}

#[tokio::test]
async fn restart_resumes_due_work_and_does_not_repeat_completed_run() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    f.due(&item).await;
    let reopened = SqliteStore::open(f.dir.path()).await.unwrap();
    let worker = Monitor::new(
        reopened.clone(),
        Settings::new(
            reopened,
            EncryptionKey::load_or_create(f.dir.path()).await.unwrap(),
        ),
    );
    assert!(worker.tick().await.unwrap());
    assert!(!worker.tick().await.unwrap());
    let item = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(item.last_state, "awaiting_release");
    assert_eq!(item.next_run % 900, 0);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failures_are_distinct_from_empty_and_candidates_never_download() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    f.due(&item).await;
    f.mode.store(1, Ordering::SeqCst);
    f.monitor.tick().await.unwrap();
    let failed = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(failed.last_state, "source_error");
    assert_eq!(failed.reason.as_deref(), Some("source_unavailable"));
    f.due(&failed).await;
    f.mode.store(0, Ordering::SeqCst);
    f.monitor.tick().await.unwrap();
    let empty = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(empty.last_state, "awaiting_release");
    f.due(&empty).await;
    f.mode.store(2, Ordering::SeqCst);
    f.monitor.tick().await.unwrap();
    let review = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(review.last_state, "needs_review");
    assert_eq!(review.candidates.len(), 1);
    let json = serde_json::to_string(&review).unwrap();
    assert!(!json.contains("private.invalid"));
    assert!(!json.contains("hidden"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM acquisition_intents")
            .fetch_one(f.store.reader())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn concurrent_claims_and_expired_lease_recovery_are_fenced() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    f.due(&item).await;
    let other_store = SqliteStore::open(f.dir.path()).await.unwrap();
    let other = Monitor::new(other_store, f.settings.clone());
    let (a, b) = tokio::join!(f.monitor.claim(), other.claim());
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let old = &claims[0];
    let mut tx = f.store.begin_write().await.unwrap();
    sqlx::query("UPDATE monitors SET lease_until = 0 WHERE id = ?")
        .bind(&item.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let recovered = other.claim().await.unwrap().unwrap();
    assert!(
        !f.monitor
            .finish(
                old,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
    assert!(
        other
            .finish(
                &recovered,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn disable_and_delete_midflight_prevent_publication_and_overlap() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    let due = f.due(&item).await;
    let claim = f.monitor.claim().await.unwrap().unwrap();
    let disabled = f
        .monitor
        .update(
            "owner",
            &item.id,
            UpdateMonitor {
                revision: due.revision,
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !f.monitor
            .finish(
                &claim,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
    assert_eq!(
        f.monitor.get("owner", &item.id).await.unwrap().last_state,
        "disabled"
    );
    let enabled = f
        .monitor
        .update(
            "owner",
            &item.id,
            UpdateMonitor {
                revision: disabled.revision,
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let due = f.due(&enabled).await;
    let claim = f.monitor.claim().await.unwrap().unwrap();
    f.monitor
        .delete("owner", &item.id, due.revision)
        .await
        .unwrap();
    let replacement = f.monitor.create("owner", f.request()).await.unwrap();
    f.due(&replacement).await;
    assert!(f.monitor.claim().await.unwrap().is_none());
    assert!(
        !f.monitor
            .finish(
                &claim,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
    assert!(f.monitor.claim().await.unwrap().is_some());
    assert!(matches!(
        f.monitor.get("owner", &item.id).await,
        Err(MonitorError::NotFound)
    ));
}

#[tokio::test]
async fn validates_scope_policy_pagination_and_optimistic_revision() {
    let f = Fixture::new().await;
    let mut request = f.request();
    request.interval_seconds = 899;
    assert!(matches!(
        f.monitor.create("owner", request).await,
        Err(MonitorError::Invalid)
    ));
    let legacy = serde_json::from_value::<CreateMonitor>(serde_json::json!({
        "unit_id": UNIT, "integration_id": f.source, "query": "Fixture 12.5",
        "interval_seconds": 900, "enabled": true, "selection_policy": "review_only"
    }))
    .unwrap();
    let item = f.monitor.create("owner", legacy).await.unwrap();
    assert_eq!(item.schedule, MonitorSchedule::Interval);
    assert_eq!(item.timezone, "UTC");
    assert!(matches!(
        f.monitor.create("owner", f.request()).await,
        Err(MonitorError::Conflict)
    ));
    assert!(matches!(
        f.monitor.get("other", &item.id).await,
        Err(MonitorError::NotFound)
    ));
    assert!(matches!(
        f.monitor.list("owner", 101, None).await,
        Err(MonitorError::Invalid)
    ));
    let current = f.due(&item).await;
    assert!(matches!(
        f.monitor.run("owner", &item.id, item.revision).await,
        Err(MonitorError::Conflict)
    ));
    assert!(matches!(
        f.monitor.delete("owner", &item.id, item.revision).await,
        Err(MonitorError::Conflict)
    ));
    assert_eq!(
        f.monitor.get("owner", &item.id).await.unwrap().revision,
        current.revision
    );
    assert!(serde_json::from_value::<CreateMonitor>(serde_json::json!({"unit_id":"u","integration_id":f.source,"query":"x","interval_seconds":900,"enabled":true,"selection_policy":"automatic"})).is_err());
}

#[tokio::test]
async fn cooldown_survives_restart_and_defers_without_contacting_source() {
    for schedule in [
        MonitorSchedule::Interval,
        MonitorSchedule::Daily {
            time: "09:00".into(),
        },
        MonitorSchedule::Weekly {
            time: "09:00".into(),
            weekday: 1,
        },
    ] {
        let f = Fixture::new().await;
        let mut request = f.request();
        request.schedule = schedule.clone();
        let item = f.monitor.create("owner", request).await.unwrap();
        f.due(&item).await;
        f.monitor.tick().await.unwrap();
        let item = f.monitor.get("owner", &item.id).await.unwrap();
        f.monitor
            .run("owner", &item.id, item.revision)
            .await
            .unwrap();
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("UPDATE search_cooldowns SET next_at = unixepoch() + 3601")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let reopened = SqliteStore::open(f.dir.path()).await.unwrap();
        let worker = Monitor::new(reopened, f.settings.clone());
        assert!(worker.tick().await.unwrap());
        let item = worker.get("owner", &item.id).await.unwrap();
        assert_eq!(item.last_state, "source_error");
        assert_eq!(item.reason.as_deref(), Some("source_cooldown"));
        assert!(item.next_run >= now() + 3599);
        assert_eq!(
            schedule.next_run(item.next_run - 1, 900, "UTC").unwrap(),
            item.next_run
        );
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn source_changes_and_query_edits_cannot_publish_old_results() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    let item = f.due(&item).await;
    let claim = f.monitor.claim().await.unwrap().unwrap();
    f.settings
        .update(
            &f.source,
            crate::settings::UpdateIntegration {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        f.monitor
            .finish(
                &claim,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
    let changed = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(changed.reason.as_deref(), Some("source_changed"));
    let due = f.due(&changed).await;
    let claim = f.monitor.claim().await.unwrap().unwrap();
    let edited = f
        .monitor
        .update(
            "owner",
            &item.id,
            UpdateMonitor {
                revision: due.revision,
                query: Some("different query".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !f.monitor
            .finish(
                &claim,
                Ok(crate::search::Releases {
                    releases: vec![],
                    offset: 0,
                    total: None,
                    next_offset: None,
                    target: None,
                })
            )
            .await
            .unwrap()
    );
    let current = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(current.query, "different query");
    assert_eq!(current.revision, edited.revision);
    assert!(current.candidates.is_empty());
}

#[tokio::test]
async fn partial_page_cannot_prove_release_unavailable() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    f.due(&item).await;
    let claim = f.monitor.claim().await.unwrap().unwrap();
    f.monitor
        .finish(
            &claim,
            Ok(crate::search::Releases {
                releases: vec![],
                offset: 0,
                total: Some(50),
                next_offset: Some(20),
                target: None,
            }),
        )
        .await
        .unwrap();
    let item = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(item.last_state, "needs_review");
    assert!(item.candidates_truncated);
}

#[tokio::test]
async fn api_scopes_owner_pagination_revisions_and_schedule_only_run() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let auth = crate::auth::AuthService::new(f.store.clone());
    let login = auth
        .setup("admin", "owned monitor fixture password")
        .await
        .unwrap();
    let manage = auth
        .create_key("manage", crate::auth::Scope::Manage)
        .await
        .unwrap();
    let read = auth
        .create_key("read", crate::auth::Scope::Read)
        .await
        .unwrap();
    let app = crate::httpapi::monitor::routes(crate::httpapi::monitor::MonitorContext {
        monitor: f.monitor.clone(),
        auth: crate::httpapi::auth::AuthContext {
            service: auth,
            origin: Arc::from("http://localhost"),
        },
    });
    let json = serde_json::json!({"unit_id":UNIT,"integration_id":f.source,"query":"Fixture","interval_seconds":900,"schedule":{"kind":"weekly","time":"02:30","weekday":7},"timezone":"Europe/Berlin","enabled":true,"selection_policy":"review_only"}).to_string();
    let req = |method: &str, uri: &str, key: Option<&str>, body: &str| {
        let mut r = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(key) = key {
            r = r.header("authorization", format!("Bearer {key}"));
        }
        r.body(Body::from(body.to_string())).unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(req("GET", "/api/v1/monitors", None, ""))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(req("POST", "/api/v1/monitors", Some(&read.secret), &json))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .clone()
        .oneshot(req("POST", "/api/v1/monitors", Some(&manage.secret), &json))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let item: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = item["id"].as_str().unwrap();
    assert_eq!(item["integration_label"], "Monitor fixture");
    assert_eq!(
        item["schedule"],
        serde_json::json!({"kind":"weekly","time":"02:30","weekday":7})
    );
    assert_eq!(item["timezone"], "Europe/Berlin");
    assert!(f.monitor.get(&login.principal.user_id, id).await.is_ok());
    assert!(matches!(
        f.monitor.get(&manage.id, id).await,
        Err(MonitorError::NotFound)
    ));
    assert_eq!(
        app.clone()
            .oneshot(req(
                "GET",
                "/api/v1/monitors?limit=101",
                Some(&read.secret),
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.clone()
            .oneshot(req(
                "GET",
                "/api/v1/monitors?cursor=invalid",
                Some(&read.secret),
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let response = app
        .clone()
        .oneshot(req("GET", "/api/v1/monitors", Some(&read.secret), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["items"][0]["integration_label"], "Monitor fixture");
    let response = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/api/v1/monitors/{id}/run"),
            Some(&manage.secret),
            "{\"revision\":1}",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        app.clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/v1/monitors/{id}"),
                Some(&manage.secret),
                "{\"revision\":1,\"enabled\":false}"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/monitors/{id}?revision=2"),
                Some(&read.secret),
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/monitors/{id}?revision=2"),
                Some(&manage.secret),
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.oneshot(req(
            "PATCH",
            &format!("/api/v1/monitors/{id}"),
            Some(&manage.secret),
            "{\"revision\":2,\"enabled\":false}"
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn disable_during_real_network_search_releases_writer_and_fences_results() {
    let f = Fixture::new().await;
    let item = f.monitor.create("owner", f.request()).await.unwrap();
    let due = f.due(&item).await;
    f.mode.store(3, Ordering::SeqCst);
    let monitor = f.monitor.clone();
    let worker = tokio::spawn(async move { monitor.tick().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while f.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let disabled = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        f.monitor.update(
            "owner",
            &item.id,
            UpdateMonitor {
                revision: due.revision,
                enabled: Some(false),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    f.mode.store(2, Ordering::SeqCst);
    assert!(worker.await.unwrap().unwrap());
    let item = f.monitor.get("owner", &item.id).await.unwrap();
    assert_eq!(item.revision, disabled.revision);
    assert_eq!(item.last_state, "disabled");
    assert!(item.candidates.is_empty());
    assert!(!item.running);
}

#[tokio::test]
async fn cursor_pages_are_owner_scoped_and_unit_content_type_is_catalog_derived() {
    let f = Fixture::new().await;
    f.settings.update(&f.source, crate::settings::UpdateIntegration {
        options:Some(serde_json::json!({"indexer_id":7,"protocol":"usenet","categories":{"comics":[7030],"manga":[7020],"magazines":[7010]}})), ..Default::default()
    }).await.unwrap();
    for (id, content_type) in [
        ("comic", ContentType::Comic),
        ("manga", ContentType::Manga),
        ("magazine", ContentType::Magazine),
    ] {
        let mut tx = f.store.begin_write().await.unwrap();
        sqlx::query("INSERT INTO publications (id,content_type,title,sort_title) VALUES (?,?,'Fixture','fixture')").bind(id).bind(id).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO editions (id,publication_id,language) VALUES (?,?,'en')")
            .bind(id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO units (id,edition_id,label,kind) VALUES (?,?,'1','issue')")
            .bind(id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut request = f.request();
        request.unit_id = id.into();
        let item = f.monitor.create("owner", request).await.unwrap();
        assert_eq!(item.content_type, content_type);
    }
    f.monitor.create("other", f.request()).await.unwrap();
    let first = f.monitor.list("owner", 2, None).await.unwrap();
    let second = f
        .monitor
        .list("owner", 2, first.next_cursor.as_deref())
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(second.items.len(), 1);
    assert!(second.next_cursor.is_none());
    assert!(first.items.iter().all(|a| a.id != second.items[0].id));
}

#[tokio::test]
async fn filtered_monitor_lists_bind_cursors_and_keep_canonical_targets() {
    let f = Fixture::new().await;
    let first = f.monitor.create("owner", f.request()).await.unwrap();
    assert_eq!(first.target.publication.id, "p");
    assert_eq!(first.target.edition.id, "e");
    assert_eq!(first.target.unit.id, UNIT);
    assert_eq!(first.integration_label, "Monitor fixture");
    let updated = f
        .monitor
        .update(
            "owner",
            &first.id,
            UpdateMonitor {
                revision: first.revision,
                query: Some("Changed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.target.unit.id, UNIT);
    let ran = f
        .monitor
        .run("owner", &first.id, updated.revision)
        .await
        .unwrap();
    assert_eq!(ran.target.publication.id, "p");
    let mut tx = f.store.begin_write().await.unwrap();
    for statement in [
        "INSERT INTO publications(id,content_type,title,sort_title) VALUES('p2','comic','Second','second')",
        "INSERT INTO editions(id,publication_id,language) VALUES('e2','p2','en')",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO units(id,edition_id,label,kind) VALUES(?,'e2','2','issue')")
        .bind(UNIT2)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let second = f
        .monitor
        .create(
            "owner",
            CreateMonitor {
                unit_id: UNIT2.into(),
                enabled: false,
                ..f.request()
            },
        )
        .await
        .unwrap();
    let other = f.monitor.create("other", f.request()).await.unwrap();
    assert_eq!(
        f.monitor
            .list_filtered(
                "owner",
                100,
                None,
                MonitorFilters {
                    publication_id: Some("p".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items[0]
            .id,
        first.id
    );
    assert_eq!(
        f.monitor
            .list_filtered(
                "owner",
                100,
                None,
                MonitorFilters {
                    unit_id: Some(UNIT2.into()),
                    enabled: Some(false),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items[0]
            .id,
        second.id
    );
    assert_eq!(
        f.monitor
            .list_filtered("owner", 100, None, MonitorFilters::default())
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    let page = f
        .monitor
        .list_filtered("owner", 1, None, MonitorFilters::default())
        .await
        .unwrap();
    let cursor = page.next_cursor.unwrap();
    assert!(matches!(
        f.monitor
            .list_filtered("other", 1, Some(&cursor), MonitorFilters::default())
            .await,
        Err(MonitorError::Invalid)
    ));
    assert!(matches!(
        f.monitor
            .list_filtered(
                "owner",
                1,
                Some(&cursor),
                MonitorFilters {
                    enabled: Some(true),
                    ..Default::default()
                }
            )
            .await,
        Err(MonitorError::Invalid)
    ));
    assert!(f.monitor.list("owner", 1, Some(&first.id)).await.is_ok());
    assert!(matches!(
        f.monitor.list("owner", 1, Some(&other.id)).await,
        Err(MonitorError::Invalid)
    ));
    assert!(matches!(
        f.monitor
            .list("owner", 1, Some(&uuid::Uuid::new_v4().to_string()))
            .await,
        Err(MonitorError::Invalid)
    ));
}
