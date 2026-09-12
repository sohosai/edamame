//! controller の HTTP API

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use edamame_domain::{
    CorsOrigins, EdamameError, RegisterRequest, RegisterResponse, SegmentPutResponse, SyncRequest,
    SyncResponse, now_ms, parse_sequence_filename, validate_identifier, validate_quality,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tower_http::cors::{Any, CorsLayer};

use crate::state::{ControllerState, IngestOutcome, SegmentIngest};

/// 1 回の PUT で受け取るセグメントファイル 1 個の上限
const SEGMENT_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// worker の登録に使う JSON ボディの上限
const REGISTER_BODY_LIMIT: usize = 4 * 1024 * 1024;
/// sync の JSON ボディの上限。worker はキャッシュしている全セグメントを送るため、余裕を持たせる。
const SYNC_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// controller のルーター
pub fn router(state: Arc<ControllerState>, cors: &CorsOrigins) -> Router {
    // ブラウザからは読み取りだけ許可し、書き込み系には CORS を付けない
    // PUT の第 3 要素はセグメント識別子、GET の第 3 要素はセグメント番号のファイル名として同じ形のパスを使う
    let read = Router::new()
        .route(
            "/segments/{channel_id}/{quality}/{filename}",
            get(get_segment),
        )
        .route("/channels/{channel_id}/{file}", get(get_playlist));
    let read = match cors_layer(cors) {
        Some(layer) => read.layer(layer),
        None => read,
    };
    Router::new()
        .route(
            "/segments/{channel_id}/{quality}/{filename}",
            put(put_segment).layer(DefaultBodyLimit::max(SEGMENT_BODY_LIMIT)),
        )
        .route(
            "/workers/{worker_id}",
            put(put_worker).layer(DefaultBodyLimit::max(REGISTER_BODY_LIMIT)),
        )
        .route(
            "/workers/{worker_id}/sync",
            post(post_sync).layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT)),
        )
        .merge(read)
        .with_state(state)
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

/// リクエストの JSON を読み取る抽出子。形式が不正な入力は 400 にする。
struct ValidatedJson<T>(T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = HttpError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Json の拒否は 422 や 415 になるため、入力不正として 400 にまとめる
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(EdamameError::BadRequest(rejection.body_text()).into()),
        }
    }
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
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, self.0.message().to_owned()).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct SegmentQuery {
    duration_ms: u64,
    bandwidth: u64,
    resolution: Option<String>,
}

async fn put_segment(
    State(state): State<Arc<ControllerState>>,
    Path((channel_id, quality, identifier)): Path<(String, String, String)>,
    Query(query): Query<SegmentQuery>,
    body: Bytes,
) -> Result<Response, HttpError> {
    let outcome = state
        .ingest_segment(SegmentIngest {
            channel_id: &channel_id,
            quality: &quality,
            identifier: &identifier,
            duration_ms: query.duration_ms,
            bandwidth: query.bandwidth,
            resolution: query.resolution,
            bytes: &body,
        })
        .await?;
    let (status, sequence) = match outcome {
        IngestOutcome::Created { sequence } => (StatusCode::CREATED, sequence),
        IngestOutcome::AlreadyExists { sequence } => (StatusCode::OK, sequence),
    };
    Ok((status, Json(SegmentPutResponse { sequence })).into_response())
}

async fn get_segment(
    State(state): State<Arc<ControllerState>>,
    Path((channel_id, quality, filename)): Path<(String, String, String)>,
) -> Result<Response, HttpError> {
    validate_identifier("channel_id", &channel_id)?;
    validate_quality(&quality)?;
    let sequence = parse_sequence_filename(&filename)?;
    let bytes = state
        .store
        .read_segment(&channel_id, &quality, sequence)
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn get_playlist(
    State(state): State<Arc<ControllerState>>,
    Path((channel_id, file)): Path<(String, String)>,
) -> Result<Response, HttpError> {
    let now = now_ms();
    let body = if file == "master.m3u8" {
        state.master_playlist(&channel_id, now).await?
    } else if let Some(quality) = file.strip_suffix(".m3u8") {
        if quality.is_empty() || quality == "master" {
            return Err(EdamameError::BadRequest("quality is invalid".to_owned()).into());
        }
        state.media_playlist(&channel_id, quality, now).await?
    } else {
        return Err(EdamameError::NotFound("playlist not found".to_owned()).into());
    };
    Ok((
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        body,
    )
        .into_response())
}

async fn put_worker(
    State(state): State<Arc<ControllerState>>,
    Path(worker_id): Path<String>,
    ValidatedJson(request): ValidatedJson<RegisterRequest>,
) -> Result<Response, HttpError> {
    let created = state
        .register_worker(&worker_id, &request.public_url, now_ms())
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(RegisterResponse {
            sync_interval_ms: state.sync_interval_ms,
        }),
    )
        .into_response())
}

async fn post_sync(
    State(state): State<Arc<ControllerState>>,
    Path(worker_id): Path<String>,
    ValidatedJson(request): ValidatedJson<SyncRequest>,
) -> Result<Json<SyncResponse>, HttpError> {
    let response = state
        .sync_worker(&worker_id, request.cached_segments, now_ms())
        .await?;
    Ok(Json(response))
}
