//! HTTP API で使う JSON の型を定義する

use serde::{Deserialize, Serialize};

use crate::segment::SegmentKey;

/// セグメントファイルの登録レスポンスを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentPutResponse {
    pub sequence: u64,
}

/// worker の登録リクエストを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub public_url: String,
    /// worker の重みを指定する
    pub weight: u32,
}

/// worker の登録レスポンスを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub sync_interval_ms: u64,
}

/// worker の sync リクエストを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub cached_segments: Vec<SegmentKey>,
    /// worker が取得中のセグメント。controller が同じ番号に保存し直したセグメントの再取得を判断するために使う
    pub fetching_segments: Vec<SegmentKey>,
}

/// worker の sync レスポンスを表す
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub fetch_segments: Vec<SegmentKey>,
}
