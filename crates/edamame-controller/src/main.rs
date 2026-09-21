//! `edamame-controller` の起動処理

use clap::Parser;
use edamame_controller::config::{ControllerConfig, DEFAULT_PUBLISH_WAIT_MS};
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
    if config.publish_wait_ms.is_none() {
        eprintln!(
            "warning: --publish-wait-ms is not specified; using {DEFAULT_PUBLISH_WAIT_MS} ms"
        );
    }
    if config.auth_token.is_none() {
        eprintln!("warning: --auth-token is not specified; PUT requests are not authenticated");
    }
    let cors = config.cors_origins()?;
    if config.cors_origin.is_empty() {
        eprintln!(
            "warning: --cors-origin is not specified; cross-origin browser access is disabled"
        );
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
    let app = router(state, &cors, config.auth_token.as_deref());
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    println!("edamame-controller listening on {}", config.bind);
    // 外部配信への振り分けで接続元を使うため、接続情報を付けて配信する
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
