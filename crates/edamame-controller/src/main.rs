//! `edamame-controller` の起動処理

use std::sync::Arc;

use clap::Parser;
use edamame_controller::cluster::ClusterState;
use edamame_controller::config::{ControllerConfig, DEFAULT_PUBLISH_WAIT_MS};
use edamame_controller::http::router;
use edamame_controller::state::ControllerState;
use edamame_controller::storage::CanonicalStore;
use edamame_domain::now_ms;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ControllerConfig::parse();
    config.validate()?;
    if config.sync_timeout_ms < config.sync_interval_ms.saturating_mul(3) {
        eprintln!("warning: sync timeout is less than 3 times the sync interval");
    }
    if config.publish_wait_ms.is_none() {
        eprintln!(
            "warning: --publish-wait-ms is not specified; using {DEFAULT_PUBLISH_WAIT_MS} ms"
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
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    println!("edamame-controller listening on {}", config.bind);
    let health = cluster.health().await;
    println!(
        "controller public_url={} metric={} role={:?}",
        health.public_url, health.metric, health.role
    );
    tokio::spawn(cluster.run());
    // 外部配信への振り分けで接続元を使うため、接続情報を付けて配信する
    let serve_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;
    if let Some(task) = metrics_task {
        task.abort();
        let _ = task.await;
    }
    if telemetry.is_enabled() {
        state.record_metrics(now_ms()).await;
    }
    telemetry.shutdown();
    serve_result?;
    Ok(())
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
