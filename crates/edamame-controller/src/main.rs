//! `edamame-controller` の起動処理

use clap::Parser;
use edamame_controller::config::ControllerConfig;
use edamame_controller::http::router;
use edamame_controller::state::ControllerState;
use edamame_controller::storage::CanonicalStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ControllerConfig::parse();
    config.validate()?;
    if config.sync_timeout_ms < config.sync_interval_ms.saturating_mul(3) {
        eprintln!("warning: sync timeout is less than 3 times the sync interval");
    }
    let cors = config.cors_origins()?;
    if config.cors_origin.is_empty() {
        eprintln!(
            "warning: --cors-origin is not specified; cross-origin browser access is disabled"
        );
    }
    tokio::fs::create_dir_all(&config.storage_dir).await?;
    let store = CanonicalStore::new(config.storage_dir.clone());
    let state = ControllerState::new(
        store,
        config.playlist_segments,
        config.sync_interval_ms,
        config.sync_timeout_ms,
    );
    let app = router(state, &cors);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    println!("edamame-controller listening on {}", config.bind);
    axum::serve(listener, app).await?;
    Ok(())
}
