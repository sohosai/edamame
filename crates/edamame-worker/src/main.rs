//! `edamame-worker` の起動処理

use clap::Parser;
use edamame_domain::normalize_http_url;
use edamame_worker::cache::Cache;
use edamame_worker::config::WorkerConfig;
use edamame_worker::http::{WorkerState, router};
use edamame_worker::sync::run_sync_loop;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = WorkerConfig::parse();
    config.validate()?;
    let cors = config.cors_origins()?;
    if config.cors_origin.is_empty() {
        eprintln!(
            "warning: --cors-origin is not specified; cross-origin browser access is disabled"
        );
    }
    let public_url = normalize_http_url(&config.public_url)?;
    let controller_url = normalize_http_url(&config.controller_url)?;
    tokio::fs::create_dir_all(&config.cache_dir).await?;
    let cache = Cache::scan(config.cache_dir.clone(), config.cache_limit).await?;
    let state = WorkerState::new(config.worker_id.clone(), controller_url, public_url, cache)?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let app = router(state.clone(), &cors);
    let sync_state = state.clone();
    tokio::spawn(async move {
        run_sync_loop(sync_state, config.auth_token).await;
    });
    println!(
        "edamame-worker {} listening on {}",
        config.worker_id, config.bind
    );
    axum::serve(listener, app).await?;
    Ok(())
}
