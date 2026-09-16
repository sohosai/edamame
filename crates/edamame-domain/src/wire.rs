//! HTTP API で使う JSON の型

use serde::{Deserialize, Serialize};

use crate::segment::SegmentKey;

/// セグメントファイルの登録レスポンス
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentPutResponse {
    pub sequence: u64,
}

/// worker の登録リクエスト
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub public_url: String,
    /// worker の相対的な配信能力
    pub weight: u32,
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
    pub fetch_segments: Vec<SegmentKey>,
}
