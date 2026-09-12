//! controller と worker の間で使う JSON の型

use serde::{Deserialize, Serialize};

use crate::segment::SegmentKey;
use crate::state::WorkerStateKind;

/// worker の登録リクエスト
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub public_url: String,
}

/// worker の登録レスポンス
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub sync_interval_ms: u64,
}

/// worker の sync リクエスト
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub cached_segments: Vec<SegmentKey>,
}

/// worker の sync レスポンス
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub state: WorkerStateKind,
    pub fetch_segments: Vec<SegmentKey>,
}
