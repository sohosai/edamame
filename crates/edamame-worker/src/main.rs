//! `edamame-worker` の起動処理

use clap::Parser;
use edamame_domain::{normalize_public_url, now_ms};
use edamame_worker::cache::Cache;
use edamame_worker::config::{TlsMode, WorkerConfig};
use edamame_worker::http::{WorkerState, router};
use edamame_worker::sync::run_sync_loop;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = WorkerConfig::parse();
    config.validate()?;
    let tls = config.tls_mode()?;
    let cors = config.cors_origins()?;
    if config.cors_origin.is_empty() {
        eprintln!(
            "warning: --cors-origin is not specified; cross-origin browser access is disabled"
        );
    }
    if let TlsMode::Acme { contact: None, .. } = &tls {
        eprintln!(
            "warning: --acme-contact is not specified; certificate problem notices cannot be sent"
        );
    }
    let mut telemetry = edamame_telemetry::init("edamame-worker", &config.worker_id);
    if telemetry.is_enabled() {
        println!(
            "OpenTelemetry metrics enabled (worker: {})",
            config.worker_id
        );
    }
    // client へは TLS 終端の reverse proxy 越しに配信することがあるため、https も許可する
    let public_url = normalize_public_url(&config.public_url)?;
    // controller も TLS を終端できるため、https を許可する
    let mut controller_urls = Vec::new();
    for url in &config.controller_urls {
        let url = normalize_public_url(url)?;
        if !controller_urls.contains(&url) {
            controller_urls.push(url);
        }
    }
    let cache = Cache::new(config.cache_limit)?;
    let state = WorkerState::new(
        config.worker_id.clone(),
        controller_urls,
        public_url,
        config.weight,
        cache,
    )?;
    let app = router(state.clone(), &cors);
    let bind = config.bind;
    let auth_token = config.auth_token.clone();
    let run_sync = move |state: std::sync::Arc<WorkerState>| {
        let auth_token = auth_token.clone();
        tokio::spawn(run_sync_loop(state, auth_token))
    };
    let metrics_task = telemetry.is_enabled().then(|| state.spawn_metrics_loop());
    println!("edamame-worker {} listening on {}", config.worker_id, bind);

    let result: Result<(), Box<dyn std::error::Error>> = async {
        match tls {
            TlsMode::Plain => {
                // 待ち受けに成功してから controller に登録する
                let listener = tokio::net::TcpListener::bind(bind).await?;
                run_sync(state.clone());
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown_signal())
                    .await?;
            }
            TlsMode::Static { cert, key } => {
                // 証明書を読み込んでから controller に登録する
                let config = edamame_tls::static_config(cert, key).await?;
                let listener = edamame_tls::bind_listener(bind)?;
                run_sync(state.clone());
                edamame_tls::serve_static_until(listener, app, config, shutdown_signal()).await?;
            }
            TlsMode::Acme {
                domain,
                directory,
                cache_dir,
                contact,
            } => {
                let listener = edamame_tls::bind_listener(bind)?;
                let (valid_sender, mut valid_receiver) = tokio::sync::watch::channel(false);
                let server = tokio::spawn(async move {
                    edamame_tls::serve_acme_until(
                        listener,
                        app,
                        &domain,
                        &directory,
                        cache_dir,
                        contact.as_deref(),
                        valid_sender,
                        shutdown_signal(),
                    )
                    .await
                });
                // 証明書が有効な間だけ controller に登録する
                let monitor_state = state.clone();
                let monitor_token = config.auth_token.clone();
                tokio::spawn(async move {
                    let mut sync_task: Option<tokio::task::JoinHandle<()>> = None;
                    let mut shutdown: Option<tokio::sync::watch::Sender<bool>> = None;
                    while valid_receiver.changed().await.is_ok() {
                        let valid = *valid_receiver.borrow();
                        if valid && sync_task.is_none() {
                            let (sender, receiver) = tokio::sync::watch::channel(false);
                            shutdown = Some(sender);
                            let state = monitor_state.clone();
                            let token = monitor_token.clone();
                            sync_task = Some(tokio::spawn(async move {
                                edamame_worker::sync::run_sync_loop_until(state, token, receiver)
                                    .await;
                            }));
                        } else if !valid && let Some(sender) = shutdown.take() {
                            let _ = sender.send(true);
                            // 取得中のタスクの停止を待ってから再開できるようにする
                            if let Some(task) = sync_task.take() {
                                let _ = task.await;
                            }
                        }
                    }
                });
                server.await??;
            }
        }
        Ok(())
    }
    .await;

    if let Some(task) = metrics_task {
        task.abort();
        let _ = task.await;
    }
    if telemetry.is_enabled() {
        state.record_metrics(now_ms()).await;
    }
    telemetry.shutdown();
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                eprintln!("warning: failed to install SIGTERM handler: {error}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("warning: failed to install Ctrl-C handler: {error}");
        }
    }
}
