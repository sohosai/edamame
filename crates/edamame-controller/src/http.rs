//! controller の HTTP API

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Extension, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use edamame_domain::{
    CorsOrigins, EdamameError, RegisterRequest, RegisterResponse, SegmentPutResponse, SyncRequest,
    SyncResponse, now_ms, parse_sequence_filename, validate_identifier, validate_quality,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tower_http::cors::{Any, CorsLayer};

use crate::state::{
    ControllerState, ExternalRouting, IngestOutcome, MasterResponse, SegmentIngest,
};

/// 1 回の PUT で受け取るセグメントファイル 1 個の上限
const SEGMENT_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// worker の登録に使う JSON ボディの上限
const REGISTER_BODY_LIMIT: usize = 4 * 1024 * 1024;
/// sync の JSON ボディの上限。worker はキャッシュしている全セグメントを送るため、余裕を持たせる。
const SYNC_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// 外部配信の登録に使う JSON ボディの上限
const EXTERNAL_BODY_LIMIT: usize = 64 * 1024;

/// controller のルーター
pub fn router(state: Arc<ControllerState>, cors: &CorsOrigins, auth_token: Option<&str>) -> Router {
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
    let write = Router::new()
        .route(
            "/segments/{channel_id}/{quality}/{filename}",
            put(put_segment).layer(DefaultBodyLimit::max(SEGMENT_BODY_LIMIT)),
        )
        .route(
            "/workers/{worker_id}",
            put(put_worker).layer(DefaultBodyLimit::max(REGISTER_BODY_LIMIT)),
        )
        .route(
            "/channels/{channel_id}/external",
            put(put_channel_external).layer(DefaultBodyLimit::max(EXTERNAL_BODY_LIMIT)),
        );
    // 認証は書き込み系の PUT だけに要求し、読み取りと sync には要求しない
    let write = match auth_token {
        Some(token) => write.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            authorize_put,
        )),
        None => write,
    };
    let sync = Router::new().route(
        "/workers/{worker_id}/sync",
        post(post_sync).layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT)),
    );
    write.merge(sync).merge(read).with_state(state)
}

/// PUT の Authorization ヘッダーを検証する
async fn authorize_put(State(expected): State<Arc<str>>, request: Request, next: Next) -> Response {
    let authorized =
        bearer_token(request.headers()).is_some_and(|provided| tokens_match(provided, &expected));
    if authorized {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized",
    )
        .into_response()
}

/// Authorization ヘッダーから Bearer トークンを取り出す
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}

/// 一致した文字数からトークンを推測されないよう、すべてのバイトを比較する
fn tokens_match(provided: &str, expected: &str) -> bool {
    let (provided, expected) = (provided.as_bytes(), expected.as_bytes());
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
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
    key_uri: Option<String>,
}

/// プレイリスト取得の query
#[derive(Debug, Deserialize)]
struct PlaylistQuery {
    /// `1` で外部配信へ振り分け、`0` で自前のマスタープレイリストを返す。省略時は接続元ごとの割合で判定する
    external: Option<u8>,
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
            key_uri: query.key_uri.as_deref(),
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
    Query(query): Query<PlaylistQuery>,
    // 接続情報を付けずに router を使う場合でも、プレイリストは返せるようにする
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Result<Response, HttpError> {
    // ルーターは名前付きパラメータの後ろに固定の文字列を置けないため、
    // `/channels/{channel_id}/{quality}.m3u8` をルートにできず、ここで分岐する
    if file == "master.m3u8" {
        let client_ip = connect_info.map(|Extension(ConnectInfo(client))| client.ip());
        return get_master_playlist(&state, &channel_id, query, client_ip).await;
    }
    let Some(quality) = file.strip_suffix(".m3u8") else {
        return Err(EdamameError::NotFound("playlist not found".to_owned()).into());
    };
    get_media_playlist(&state, &channel_id, quality).await
}

/// マスタープレイリストを返す。外部配信へ振り分ける場合は転送する
async fn get_master_playlist(
    state: &ControllerState,
    channel_id: &str,
    query: PlaylistQuery,
    client_ip: Option<IpAddr>,
) -> Result<Response, HttpError> {
    let routing = match query.external {
        Some(0) => ExternalRouting::LocalOnly,
        Some(1) => ExternalRouting::ExternalOnly,
        Some(_) => {
            return Err(EdamameError::BadRequest("external must be 0 or 1".to_owned()).into());
        }
        None => match client_ip {
            Some(client_ip) => ExternalRouting::ByClient(client_ip),
            // 接続情報を付けずに router を使う場合は外部へ振り分けない
            None => ExternalRouting::LocalOnly,
        },
    };
    let body = match state.master_playlist(channel_id, now_ms(), routing).await? {
        MasterResponse::Local(body) => body,
        MasterResponse::External(url) => {
            let location = HeaderValue::from_str(&url).map_err(|_| {
                EdamameError::Internal("external m3u8 is not a valid header value".to_owned())
            })?;
            return Ok((
                StatusCode::FOUND,
                [
                    (header::LOCATION, location),
                    // 接続元ごとの判定が中間キャッシュに固定されないようにする
                    (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                ],
            )
                .into_response());
        }
    };
    // マスタープレイリストは接続元ごとに変わるため、共有キャッシュに固定されないようにする
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.apple.mpegurl"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response())
}

/// メディアプレイリストを返す
async fn get_media_playlist(
    state: &ControllerState,
    channel_id: &str,
    quality: &str,
) -> Result<Response, HttpError> {
    let body = state.media_playlist(channel_id, quality, now_ms()).await?;
    Ok((
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        body,
    )
        .into_response())
}

/// channel の外部配信の登録リクエスト
#[derive(Debug, Deserialize)]
struct ExternalM3u8Request {
    /// 外部配信の master m3u8 の URL。`null` で取り消す
    external_m3u8: Option<String>,
}

async fn put_channel_external(
    State(state): State<Arc<ControllerState>>,
    Path(channel_id): Path<String>,
    ValidatedJson(request): ValidatedJson<ExternalM3u8Request>,
) -> Result<Response, HttpError> {
    state
        .set_external_m3u8(&channel_id, request.external_m3u8.as_deref())
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn put_worker(
    State(state): State<Arc<ControllerState>>,
    Path(worker_id): Path<String>,
    ValidatedJson(request): ValidatedJson<RegisterRequest>,
) -> Result<Response, HttpError> {
    let created = state
        .register_worker(&worker_id, &request.public_url, request.weight, now_ms())
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
