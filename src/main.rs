use libraryd::{app, config::Config, store::sqlite::SqliteStore};
use std::{process::ExitCode, time::Duration};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("libraryd=info,warn")),
        )
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start runtime");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run());
    // A stalled network filesystem syscall cannot be canceled by dropping its future.
    runtime.shutdown_timeout(Duration::from_secs(5));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "service stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    if !libraryd::cli::dispatch(std::env::args_os().skip(1).collect()).await? {
        return Ok(());
    }
    let config = Config::from_env()?;
    SqliteStore::ensure_state_directory(&config.state_dir).await?;
    let key = libraryd::settings::EncryptionKey::load_or_create(&config.state_dir).await?;
    let store = SqliteStore::open(&config.state_dir).await?;
    let settings = libraryd::settings::Settings::new(store.clone(), key);
    settings.list().await?;
    let router = libraryd::bootstrap::protect(
        app::router_with_settings(store.clone(), &config.origin, settings.clone()),
        store.clone(),
        &config.state_dir,
        true,
    )
    .await?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let (worker_shutdown, worker_receiver) = tokio::sync::watch::channel(false);
    let mut workers = tokio::task::JoinSet::new();
    if config.workers {
        let direct_store = store.clone();
        let direct_settings = settings.clone();
        let direct_receiver = worker_receiver.clone();
        workers.spawn(async move {
            libraryd::worker::run_direct(direct_store, direct_settings, direct_receiver)
                .await
                .map_err(|error| error.to_string())
        });
        let monitor_store = store.clone();
        let monitor_settings = settings.clone();
        let monitor_receiver = worker_receiver.clone();
        workers.spawn(async move {
            libraryd::worker::run_monitors(monitor_store, monitor_settings, monitor_receiver)
                .await
                .map_err(|error| error.to_string())
        });
        let scan_store = store.clone();
        let scan_receiver = worker_receiver.clone();
        workers.spawn(async move {
            libraryd::worker::run(scan_store, scan_receiver)
                .await
                .map_err(|error| error.to_string())
        });
        let acquisition_store = store.clone();
        let acquisition_settings = settings.clone();
        workers.spawn(async move {
            libraryd::worker::run_acquisition(
                acquisition_store,
                acquisition_settings,
                worker_receiver,
            )
            .await
            .map_err(|error| error.to_string())
        });
    } else {
        tracing::warn!("background workers disabled by configuration");
    }
    tracing::info!(address = %listener.local_addr()?, "service listening");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut server = tokio::spawn(
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .into_future(),
    );
    let server_result: Result<(), Box<dyn std::error::Error>> = tokio::select! {
        result = workers.join_next(), if !workers.is_empty() => {
            let _ = shutdown_tx.send(());
            server.abort();
            let _ = server.await;
            result.ok_or_else(|| "no background workers remain".into())
                .and_then(|result| result.map_err(Into::into))
                .and_then(|result| result.map_err(Into::into))
                .and_then(|()| Err("job worker stopped unexpectedly".into()))
        }
        result = &mut server => { result.map_err(Into::into).and_then(|result| result.map_err(Into::into)) }
        signal = shutdown_signal() => {
            let _ = shutdown_tx.send(());
            match tokio::time::timeout(Duration::from_secs(10), &mut server).await {
                Ok(result) => signal.map_err(Into::into)
                    .and_then(|()| result.map_err(Into::into))
                    .and_then(|result| result.map_err(Into::into)),
                Err(_) => {
                    server.abort();
                    let _ = server.await;
                    Err("HTTP shutdown exceeded 10 seconds".into())
                }
            }
        }
    };
    let _ = worker_shutdown.send(true);
    let worker_result: Result<(), Box<dyn std::error::Error>> =
        match tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(result) = workers.join_next().await {
                result.map_err(|error| error.to_string())??;
            }
            Ok::<(), String>(())
        })
        .await
        {
            Ok(result) => result.map_err(Into::into),
            Err(_) => {
                workers.shutdown().await;
                Err("job worker shutdown exceeded 10 seconds".into())
            }
        };
    tokio::time::timeout(Duration::from_secs(5), store.close())
        .await
        .map_err(|_| "database shutdown exceeded 5 seconds")?;
    server_result.and(worker_result)
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
