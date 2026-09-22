//! worker の HTTP API

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use edamame_domain::{
    CorsOrigins, EdamameError, parse_sequence_filename, validate_identifier, validate_quality,
};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::cache::Cache;

/// worker の共有状態
pub struct WorkerState {
    pub worker_id: String,
    pub controller_url: String,
    pub public_url: String,
    pub weight: u32,
    pub path_prefix: String,
    pub cache: Mutex<Cache>,
    pub client: reqwest::Client,
}

impl WorkerState {
    /// HTTP client と共有状態を作る
    pub fn new(
        worker_id: String,
        controller_url: String,
        public_url: String,
        weight: u32,
        cache: Cache,
    ) -> Result<Arc<Self>, EdamameError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                EdamameError::Internal(format!("build http client failed: {error}"))
            })?;
        let parsed = reqwest::Url::parse(&public_url)
            .map_err(|error| EdamameError::Internal(format!("invalid public_url: {error}")))?;
        let path_prefix = parsed.path().trim_end_matches('/').to_owned();
        Ok(Arc::new(Self {
            worker_id,
            controller_url,
            public_url,
            weight,
            path_prefix,
            cache: Mutex::new(cache),
            client,
        }))
    }
}

/// worker のルーター
pub fn router(state: Arc<WorkerState>, cors: &CorsOrigins) -> Router {
    let route = format!(
        "{}/segments/{{channel_id}}/{{quality}}/{{filename}}",
        state.path_prefix
    );
    // パスの接頭辞は URL のリテラルなので、`:` や `*` を axum 7 の記法として解釈させない
    let router = Router::new()
        .without_v07_checks()
        .route(&route, get(get_segment));
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
        Some(bytes) => Ok(segment_response(bytes)),
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
