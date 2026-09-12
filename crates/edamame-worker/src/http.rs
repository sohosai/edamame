//! worker の HTTP API

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use edamame_domain::{
    EdamameError, parse_sequence_filename, validate_identifier, validate_quality,
};
use tokio::sync::Mutex;

use crate::cache::Cache;

/// worker の共有状態
pub struct WorkerState {
    pub worker_id: String,
    pub controller_url: String,
    pub public_url: String,
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
            path_prefix,
            cache: Mutex::new(cache),
            client,
        }))
    }
}

/// worker のルーター
pub fn router(state: Arc<WorkerState>) -> Router {
    let route = format!(
        "{}/segments/{{channel_id}}/{{quality}}/{{filename}}",
        state.path_prefix
    );
    // パスの接頭辞は URL のリテラルなので、`:` や `*` を axum 7 の記法として解釈させない
    Router::new()
        .without_v07_checks()
        .route(&route, get(get_segment))
        .with_state(state)
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
    let path = {
        let cache = state.cache.lock().await;
        if !cache.contains(&channel_id, &quality, sequence) {
            return Err(EdamameError::NotFound("segment is not cached".to_owned()).into());
        }
        cache.path_for(&channel_id, &quality, sequence)?
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
        }
        Err(error) => {
            // 読めないセグメントファイルを索引に残すと、次の sync で取得対象に戻らないため索引から外す
            let mut cache = state.cache.lock().await;
            cache.remove(&channel_id, &quality, sequence);
            if error.kind() == std::io::ErrorKind::NotFound {
                Err(EdamameError::NotFound("segment is not cached".to_owned()).into())
            } else {
                Err(EdamameError::Internal(format!("read cached segment failed: {error}")).into())
            }
        }
    }
}
