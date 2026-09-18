//! Edamame の controller と worker で共有するドメイン型と純粋な処理

pub mod cors;
pub mod error;
pub mod playlist;
pub mod segment;
pub mod token;
pub mod url;
pub mod wire;

pub use cors::CorsOrigins;
pub use error::EdamameError;
pub use playlist::{
    KeyChange, MasterQuality, MediaPlaylist, QualityPlaylistInput, key_changes_for_segments,
    plan_master_playlist, plan_media_playlist, record_key_change, render_master_playlist,
    render_media_playlist,
};
pub use segment::{
    SegmentId, SegmentKey, parse_sequence_filename, render_filename, validate_identifier,
    validate_quality, validate_resolution,
};
pub use token::validate_auth_token;
pub use url::{normalize_external_m3u8, normalize_http_url, normalize_key_uri};
pub use wire::{RegisterRequest, RegisterResponse, SegmentPutResponse, SyncRequest, SyncResponse};

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix エポックからの現在時刻をミリ秒で返す
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
