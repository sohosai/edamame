//! worker の HTTP API

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use edamame_domain::{
    CorsOrigins, EdamameError, now_ms, parse_sequence_filename, validate_identifier,
    validate_quality,
};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::cache::Cache;
use crate::metrics::WorkerMetrics;

/// worker の共有状態
pub struct WorkerState {
    pub worker_id: String,
    pub controller_urls: Vec<String>,
    pub public_url: String,
    pub weight: u32,
    pub path_prefix: String,
    pub cache: Mutex<Cache>,
    pub client: reqwest::Client,
    pub(crate) metrics: WorkerMetrics,
    last_controller_success_ms: AtomicU64,
}

impl WorkerState {
    /// HTTP client と共有状態を作る
    pub fn new(
        worker_id: String,
        controller_urls: Vec<String>,
        public_url: String,
        weight: u32,
        cache: Cache,
    ) -> Result<Arc<Self>, EdamameError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|error| {
                EdamameError::Internal(format!("build http client failed: {error}"))
            })?;
        let parsed = reqwest::Url::parse(&public_url)
            .map_err(|error| EdamameError::Internal(format!("invalid public_url: {error}")))?;
        let path_prefix = parsed.path().trim_end_matches('/').to_owned();
        Ok(Arc::new(Self {
            worker_id,
            controller_urls,
            public_url,
            weight,
            path_prefix,
            cache: Mutex::new(cache),
            client,
            metrics: WorkerMetrics::new(),
            last_controller_success_ms: AtomicU64::new(now_ms()),
        }))
    }

    /// controller への登録または sync に成功した時刻を記録する
    pub(crate) fn record_controller_success(&self, at_ms: u64) {
        self.last_controller_success_ms
            .store(at_ms, Ordering::Relaxed);
    }

    fn controller_last_success_ms(&self) -> u64 {
        self.last_controller_success_ms.load(Ordering::Relaxed)
    }

    /// 現在の状態をメトリクスに記録する
    pub async fn record_metrics(&self, now_ms: u64) {
        let stats = {
            let cache = self.cache.lock().await;
            cache.stats()
        };
        self.metrics.record_up();
        self.metrics.record_controller_last_success_age(age_seconds(
            now_ms,
            self.controller_last_success_ms(),
        ));
        for stat in stats {
            self.metrics.record_cache(
                &stat.channel_id,
                &stat.quality,
                stat.head_sequence,
                u64::try_from(stat.segments).unwrap_or(u64::MAX),
            );
        }
    }

    /// 5 秒ごとに状態をメトリクスへ記録するタスクを起動する
    #[must_use = "終了時に abort するため、JoinHandle を保持すること"]
    pub fn spawn_metrics_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                state.record_metrics(now_ms()).await;
            }
        })
    }
}

fn age_seconds(now_ms: u64, then_ms: u64) -> f64 {
    now_ms.saturating_sub(then_ms) as f64 / 1000.0
}

/// worker のルーター
pub fn router(state: Arc<WorkerState>, cors: &CorsOrigins) -> Router {
    let route = format!(
        "{}/segments/{{channel_id}}/{{quality}}/{{filename}}",
        state.path_prefix
    );
    // パスの接頭辞は URL のリテラルなので、`:` や `*` を axum 7 の記法として解釈させない
    let mut router = Router::new()
        .without_v07_checks()
        .route("/health", get(health))
        .route(&route, get(get_segment));
    // リバースプロキシが公開 URL のパス接頭辞ごと転送する場合にも health を確認できるようにする
    if !state.path_prefix.is_empty() {
        router = router.route(&format!("{}/health", state.path_prefix), get(health));
    }
    let router = match cors_layer(cors) {
        Some(layer) => router.layer(layer),
        None => router,
    };
    router.with_state(state)
}

/// 許可する origin から読み取り用の CORS 層を作る
fn cors_layer(cors: &CorsOrigins) -> Option<CorsLayer> {
    let layer = match cors {
        CorsOrigins::Disabled => return None,
        CorsOrigins::Any => CorsLayer::new().allow_origin(Any),
        CorsOrigins::List(origins) => CorsLayer::new().allow_origin(
            origins
                .iter()
                // 起動時に URL として検証済みのため、ヘッダー値への変換は失敗しない
                .map(|origin| HeaderValue::try_from(origin.as_str()).expect("validated origin"))
                .collect::<Vec<_>>(),
        ),
    };
    Some(layer.allow_methods([Method::GET]))
}

/// EdamameError を plain text の HTTP レスポンスに変換する
pub struct HttpError(EdamameError);

impl From<EdamameError> for HttpError {
    fn from(error: EdamameError) -> Self {
        Self(error)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = axum::http::StatusCode::from_u16(self.0.status_code())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        (status, self.0.message().to_owned()).into_response()
    }
}

/// プロセスが HTTP を受け付けられることを示す死活監視用の応答
async fn health() -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::OK, "ok")
}

async fn get_segment(
    State(state): State<Arc<WorkerState>>,
    Path((channel_id, quality, filename)): Path<(String, String, String)>,
) -> Result<Response, HttpError> {
    validate_identifier("channel_id", &channel_id)?;
    validate_quality(&quality)?;
    let sequence = parse_sequence_filename(&filename)?;
    let bytes = {
        let cache = state.cache.lock().await;
        cache.get(&channel_id, &quality, sequence)
    };
    match bytes {
        Some(bytes) => {
            state.metrics.record_served(&channel_id, &quality);
            Ok(segment_response(bytes))
        }
        None => Err(EdamameError::NotFound("segment is not cached".to_owned()).into()),
    }
}

/// セグメントファイルを小さな塊に分けて返す
/// 一度に大きなバイト列を書くと、遅い client への接続ごとに TLS のバッファが膨らむ
fn segment_response(bytes: bytes::Bytes) -> Response {
    const CHUNK_SIZE: usize = 64 * 1024;
    let length = bytes.len();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < length {
        let end = (start + CHUNK_SIZE).min(length);
        chunks.push(Ok::<bytes::Bytes, std::convert::Infallible>(
            bytes.slice(start..end),
        ));
        start = end;
    }
    let headers = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        ),
        (
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&length.to_string())
                .expect("segment length is a valid header value"),
        ),
    ];
    (headers, Body::from_stream(tokio_stream::iter(chunks))).into_response()
}
