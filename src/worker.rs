use crate::{
    jobs::{Job, JobError, JobState, Jobs},
    library::roots::Library,
    store::sqlite::SqliteStore,
};
use std::time::Duration;
use tokio::sync::watch;

pub async fn run_direct(
    store: SqliteStore,
    settings: crate::settings::Settings,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), crate::acquisition::direct::DirectError> {
    let direct = crate::acquisition::direct::Direct::new(store);
    loop {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            result = direct.tick(settings.clone()) => { result?; }
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

pub async fn run_monitors(
    store: SqliteStore,
    settings: crate::settings::Settings,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), crate::acquisition::monitor::MonitorError> {
    let monitor = crate::acquisition::monitor::Monitor::new(store, settings);
    loop {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            result = monitor.tick() => { result?; }
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

pub async fn run_acquisition(
    store: SqliteStore,
    settings: crate::settings::Settings,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), crate::acquisition::pipeline::PipelineError> {
    let pipeline = crate::acquisition::pipeline::Pipeline::new(store);
    loop {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            result = pipeline.tick(settings.clone()) => { result?; }
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

pub async fn run(store: SqliteStore, mut shutdown: watch::Receiver<bool>) -> Result<(), JobError> {
    let jobs = Jobs::new(store.clone());
    let library = Library::new(store);
    let worker = format!("scanner:{}", uuid::Uuid::new_v4());
    loop {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        jobs.reconcile_expired().await?;
        let Some(job) = jobs.claim_kind(&worker, 30, "library.scan").await? else {
            tokio::select! {_=tokio::time::sleep(Duration::from_secs(1))=>{},_=shutdown.changed()=>return Ok(())}
            continue;
        };
        match scan_job(
            &jobs,
            &library,
            &worker,
            &job,
            &mut shutdown,
            Duration::from_secs(5),
        )
        .await
        {
            Ok(()) => {}
            Err(JobError::Conflict | JobError::NotFound) => {
                settle_conflict(&jobs, &worker, &job.id).await?
            }
            Err(error) => return Err(error),
        }
    }
}

async fn settle_conflict(jobs: &Jobs, worker: &str, id: &str) -> Result<(), JobError> {
    if let Some(job) = jobs.get(id).await?
        && job.state == JobState::CancelRequested
        && job.worker.as_deref() == Some(worker)
    {
        match jobs.acknowledge_cancel(id, worker).await {
            Ok(_) | Err(JobError::Conflict | JobError::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn scan_job(
    jobs: &Jobs,
    library: &Library,
    worker: &str,
    job: &Job,
    shutdown: &mut watch::Receiver<bool>,
    heartbeat_period: Duration,
) -> Result<(), JobError> {
    let Some(root) = job
        .payload
        .get("root_id")
        .and_then(serde_json::Value::as_str)
    else {
        jobs.fail(&job.id, worker, "invalid_scan_payload", None)
            .await?;
        return Ok(());
    };
    // The scan must keep polling while heartbeats wait for its writer transaction.
    let mut scans = tokio::task::JoinSet::new();
    let library = library.clone();
    let root = root.to_owned();
    scans.spawn(async move { library.scan(&root).await });
    let mut heartbeat = tokio::time::interval(heartbeat_period);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _=shutdown.changed()=>return Ok(()),
            _=heartbeat.tick()=>{
                let current=jobs.get(&job.id).await?.ok_or(JobError::NotFound)?;
                if current.state==JobState::CancelRequested {
                    scans.shutdown().await;
                    jobs.acknowledge_cancel(&job.id,worker).await?;return Ok(());
                }
                jobs.heartbeat(&job.id,worker,30).await?;
            }
            result=scans.join_next()=>{
                let current=jobs.get(&job.id).await?.ok_or(JobError::NotFound)?;
                if current.state==JobState::CancelRequested {jobs.acknowledge_cancel(&job.id,worker).await?;}
                else {match result {
                    Some(Ok(Ok(report))) if report.errors==0=>{jobs.complete(&job.id,worker).await?;},
                    Some(Ok(Ok(_)))=>{jobs.fail(&job.id,worker,"scan_has_unreadable_entries",None).await?;},
                    _=>{jobs.fail(&job.id,worker,"scan_failed",None).await?;},
                }}
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap()
    }
    #[tokio::test]
    async fn cancellation_between_read_and_heartbeat_does_not_kill_worker() {
        let state = private_directory();
        let store = SqliteStore::open(state.path()).await.unwrap();
        let jobs = Jobs::new(store.clone());
        let job = jobs
            .enqueue(
                "library.scan",
                "root:test",
                serde_json::json!({"root_id":"test"}),
            )
            .await
            .unwrap();
        jobs.claim_kind("worker", 30, "library.scan").await.unwrap();
        assert_eq!(
            jobs.get(&job.id).await.unwrap().unwrap().state,
            JobState::Running
        );
        jobs.request_cancel(&job.id).await.unwrap();
        assert!(matches!(
            jobs.heartbeat(&job.id, "worker", 30).await,
            Err(JobError::Conflict)
        ));
        settle_conflict(&jobs, "worker", &job.id).await.unwrap();
        assert_eq!(
            jobs.get(&job.id).await.unwrap().unwrap().state,
            JobState::Canceled
        );
        let next = jobs
            .enqueue("library.scan", "root:next", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            jobs.claim_kind("worker", 30, "library.scan")
                .await
                .unwrap()
                .unwrap()
                .id,
            next.id
        );
    }
    #[tokio::test]
    async fn heartbeats_do_not_suspend_a_scan_holding_the_writer() {
        let state = private_directory();
        let source = tempfile::tempdir().unwrap();
        for n in 0..300 {
            std::fs::write(source.path().join(format!("{n}.cbz")), b"inventory fixture").unwrap();
        }
        let store = SqliteStore::open(state.path()).await.unwrap();
        let library = Library::new(store.clone());
        let root = library
            .register_root("fixtures", source.path())
            .await
            .unwrap();
        let jobs = Jobs::new(store.clone());
        jobs.enqueue(
            "library.scan",
            &format!("root:{}", root.id),
            serde_json::json!({"root_id":root.id}),
        )
        .await
        .unwrap();
        let job = jobs
            .claim_kind("worker", 30, "library.scan")
            .await
            .unwrap()
            .unwrap();
        let (_stop, mut shutdown) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(5),
            scan_job(
                &jobs,
                &library,
                "worker",
                &job,
                &mut shutdown,
                Duration::from_millis(1),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            jobs.get(&job.id).await.unwrap().unwrap().state,
            JobState::Completed
        );
        let beats: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM job_events WHERE job_id=? AND event='heartbeat'",
        )
        .bind(&job.id)
        .fetch_one(store.reader())
        .await
        .unwrap();
        assert!(beats > 1);
    }
}
