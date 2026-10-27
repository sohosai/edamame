//! `edamame-controller` の起動処理

use std::sync::Arc;

use clap::Parser;
use edamame_controller::cluster::ClusterState;
use edamame_controller::config::{ControllerConfig, DEFAULT_PUBLISH_WAIT_MS, TlsMode};
use edamame_controller::http::router;
use edamame_controller::state::ControllerState;
use edamame_controller::storage::CanonicalStore;
use edamame_domain::now_ms;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ControllerConfig::parse();
    config.validate()?;
    let tls = config.tls_mode()?;
    if config.sync_timeout_ms < config.sync_interval_ms.saturating_mul(3) {
        eprintln!("warning: sync timeout is less than 3 times the sync interval");
    }
    if config.publish_wait_ms.is_none() {
        eprintln!(
            "warning: --publish-wait-ms is not specified; using {DEFAULT_PUBLISH_WAIT_MS} ms"
        );
    }
    if let TlsMode::Acme { contact: None, .. } = &tls {
        eprintln!(
            "warning: --acme-contact is not specified; certificate problem notices cannot be sent"
        );
    }
    let cors = config.cors_origins()?;
    if config.cors_origin.is_empty() {
        eprintln!(
            "warning: --cors-origin is not specified; cross-origin browser access is disabled"
        );
    }
    let instance_id = instance_id();
    let mut telemetry = edamame_telemetry::init("edamame-controller", &instance_id);
    if telemetry.is_enabled() {
        println!("OpenTelemetry metrics enabled (instance: {instance_id})");
    }
    let store = CanonicalStore::new(config.storage_capacity);
    let state = ControllerState::new(
        store,
        config.playlist_segments,
        config.sync_interval_ms,
        config.sync_timeout_ms,
        config.publish_wait_ms(),
        config.external_min_percent,
    );
    let cluster = ClusterState::new(&config, state.clone())?;
    let app = router(Arc::clone(&state), &cors, cluster.clone());
    let metrics_task = telemetry.is_enabled().then(|| state.spawn_metrics_loop());
    let bind = config.bind;

    let result: Result<(), Box<dyn std::error::Error>> = async {
        match tls {
            TlsMode::Plain => {
                let listener = tokio::net::TcpListener::bind(bind).await?;
                println!("edamame-controller listening on {bind}");
                log_startup(&cluster).await;
                tokio::spawn(cluster.run());
                // 外部配信への振り分けで接続元を使うため、接続情報を付けて配信する
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            }
            TlsMode::Static { cert, key } => {
                let tls_config = edamame_tls::static_config(cert, key).await?;
                let listener = edamame_tls::bind_listener(bind)?;
                println!("edamame-controller listening on {bind}");
                log_startup(&cluster).await;
                tokio::spawn(cluster.run());
                edamame_tls::serve_static_until(listener, app, tls_config, shutdown_signal())
                    .await?;
            }
            TlsMode::Acme {
                domain,
                directory,
                cache_dir,
                contact,
            } => {
                let listener = edamame_tls::bind_listener(bind)?;
                println!("edamame-controller listening on {bind}");
                println!("waiting for acme certificate for {domain}");
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
                // 証明書が無効な間は heartbeat を止め、peer に生存と誤認させない
                let mut cluster_task: Option<tokio::task::JoinHandle<()>> = None;
                loop {
                    if *valid_receiver.borrow_and_update() {
                        if cluster_task.is_none() {
                            log_startup(&cluster).await;
                            cluster_task = Some(tokio::spawn(cluster.clone().run()));
                        }
                    } else if let Some(task) = cluster_task.take() {
                        task.abort();
                        let _ = task.await;
                        eprintln!(
                            "controller stopped the cluster heartbeat because the acme certificate is not valid"
                        );
                    }
                    if valid_receiver.changed().await.is_err() {
                        break;
                    }
                }
                if let Some(task) = cluster_task {
                    task.abort();
                    let _ = task.await;
                }
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

/// 起動時の公開 URL と既知の peer を出力する
async fn log_startup(cluster: &ClusterState) {
    let health = cluster.health().await;
    println!(
        "controller public_url={} peers={}",
        health.public_url,
        health.members.len()
    );
}

fn instance_id() -> String {
    std::env::var("EDAMAME_INSTANCE_ID")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "controller".to_owned())
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
