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

use crate::cluster::{ClusterState, Heartbeat, Role};

use crate::state::{
    ControllerState, ExternalRouting, IngestOutcome, MasterResponse, SegmentIngest,
};

/// 1 回の PUT で受け取るセグメントファイル 1 個の上限
const SEGMENT_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// worker の登録に使う JSON ボディの上限
const REGISTER_BODY_LIMIT: usize = 4 * 1024 * 1024;
/// sync の JSON ボディの上限
/// worker はキャッシュ一覧をすべて送るため、登録時より大きな上限を設ける
const SYNC_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// 外部配信の登録に使う JSON ボディの上限
const EXTERNAL_BODY_LIMIT: usize = 64 * 1024;

/// cluster を指定しない利用でも、単独 leader として共通の役割判定を使う。
pub fn router(state: Arc<ControllerState>, cors: &CorsOrigins, auth_token: Option<&str>) -> Router {
    let cluster = ClusterState::standalone(state.clone());
    router_with_cluster(state, cors, auth_token, cluster)
}

/// 認証と CORS を転送処理の外側に置き、health と heartbeat は各機で処理する。
pub fn router_with_cluster(
    state: Arc<ControllerState>,
    cors: &CorsOrigins,
    auth_token: Option<&str>,
    cluster: Arc<ClusterState>,
) -> Router {
    // ブラウザからは読み取りだけ許可し、書き込み系には CORS を付けない
    // セグメントのパスの第 3 要素は、PUT ではセグメント識別子、GET ではセグメント番号のファイル名として扱う
    let read = Router::new()
        .route(
            "/segments/{channel_id}/{quality}/{filename}",
            get(get_segment),
        )
        .route("/channels/{channel_id}/{file}", get(get_playlist));
    let read = read.layer(middleware::from_fn_with_state(
        cluster.clone(),
        route_cluster,
    ));
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
    let write = write.layer(middleware::from_fn_with_state(
        cluster.clone(),
        route_cluster,
    ));
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
    let sync = sync.layer(middleware::from_fn_with_state(
        cluster.clone(),
        route_cluster,
    ));
    let internal = Router::new()
        .route("/internal/cluster/heartbeat", post(cluster_heartbeat))
        .layer(DefaultBodyLimit::max(REGISTER_BODY_LIMIT));
    // cluster の無効時も内部 API を匿名では使わせない。
    let internal = internal.layer(middleware::from_fn_with_state(
        auth_token.map(Arc::<str>::from),
        authorize_heartbeat,
    ));
    let prefix = cluster.path_prefix();
    let control = internal
        .route("/health", get(cluster_health))
        .with_state(cluster);
    let app = write
        .merge(sync)
        .merge(read)
        .with_state(state)
        .merge(control);
    if prefix.is_empty() {
        app
    } else {
        Router::new().nest(&prefix, app)
    }
}

async fn cluster_health(State(cluster): State<Arc<ClusterState>>) -> Json<crate::cluster::Health> {
    Json(cluster.health().await)
}

async fn cluster_heartbeat(
    State(cluster): State<Arc<ClusterState>>,
    ValidatedJson(heartbeat): ValidatedJson<Heartbeat>,
) -> Result<Json<Heartbeat>, HttpError> {
    Ok(Json(cluster.receive(heartbeat).await?))
}

/// token 未設定時は認証を省略せず、内部 API を常に拒否する。
async fn authorize_heartbeat(
    State(expected): State<Option<Arc<str>>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = expected {
        return authorize_put(State(expected), request, next).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized",
    )
        .into_response()
}

/// leader 未確定や中継失敗の際に、1 秒後の再試行を促す応答。
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::RETRY_AFTER, "1"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "leader unavailable",
    )
        .into_response()
}

/// 受信側の接頭辞を除いた URI を受け取り、leader の公開 URL に連結する。
fn leader_location(leader: &str, uri: &axum::http::Uri) -> String {
    format!(
        "{leader}{}",
        uri.path_and_query().map_or("/", |value| value.as_str())
    )
}

/// leader は自分で処理し、follower は GET と sync を redirect、PUT を内部中継する。
async fn route_cluster(
    State(cluster): State<Arc<ClusterState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut inner = cluster.inner.read().await;
    if inner.role == Role::Leader && matches!(*request.method(), Method::PUT | Method::POST) {
        // 低速なアップロードで heartbeat を止めないよう、body の受信中は役割を固定しない。
        drop(inner);
        let limit = match (request.method(), request.uri().path()) {
            (&Method::PUT, path) if path.starts_with("/workers/") => REGISTER_BODY_LIMIT,
            (&Method::PUT, path) if path.starts_with("/channels/") => EXTERNAL_BODY_LIMIT,
            _ => SEGMENT_BODY_LIMIT,
        };
        let (parts, body) = request.into_parts();
        let body = match axum::body::to_bytes(body, limit).await {
            Ok(body) => body,
            Err(_) if parts.method == Method::PUT && parts.uri.path().starts_with("/segments/") => {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    "request body is too large or unreadable",
                )
                    .into_response();
            }
        };
        request = Request::from_parts(parts, axum::body::Body::from(body));
        // 受信中に降格していた場合は、新しい役割に従って転送する。
        inner = cluster.inner.read().await;
    }
    if inner.role == Role::Leader {
        // 降格はこの処理が終わるまで待ち、全消去した状態への書き戻しを防ぐ。
        let response = next.run(request).await;
        drop(inner);
        return response;
    }
    let Some(leader) = &inner.leader_url else {
        return unavailable();
    };
    let location = leader_location(leader, request.uri());
    drop(inner);
    if request.method() != Method::PUT {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [
                (header::LOCATION, location),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
        )
            .into_response();
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, SEGMENT_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let mut relay = cluster.client.request(parts.method, location).body(body);
    for name in [header::AUTHORIZATION, header::CONTENT_TYPE] {
        if let Some(value) = parts.headers.get(&name) {
            relay = relay.header(name, value);
        }
    }
    let result = async {
        let response = relay.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, headers, body).into_response())
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => {
            eprintln!("cluster relay failed: {error}");
            cluster.refresh.notify_one();
            unavailable()
        }
    }
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

/// 長さが異なる場合は比較せず `false` を返す
/// 長さが同じ場合は、一致した部分からトークンを推測されないよう、すべてのバイトを比較する
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
                // 検証済みの CorsOrigins を受け取る前提で、ヘッダー値に変換する
                .map(|origin| HeaderValue::try_from(origin.as_str()).expect("validated origin"))
                .collect::<Vec<_>>(),
        ),
    };
    Some(layer.allow_methods([Method::GET]))
}

/// JSON の形式が不正な場合に 400 を返す抽出子
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
    iv: Option<String>,
}

/// プレイリスト取得の query
#[derive(Debug, Deserialize)]
struct PlaylistQuery {
    /// `1` で外部配信へ振り分け、`0` で自前のマスタープレイリストを返す
    /// それ以外の値は 400 を返す
    /// 省略時は接続元と channel、その時点の割合で判定する
    /// 省略時に接続情報がなければ、自前のマスタープレイリストを返す
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
            iv: query.iv.as_deref(),
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
    let bytes = state.store.get(&channel_id, &quality, sequence)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn get_playlist(
    State(state): State<Arc<ControllerState>>,
    Path((channel_id, file)): Path<(String, String)>,
    Query(query): Query<PlaylistQuery>,
    // 接続情報なしでもプレイリストを取得できるよう、任意の引数として受け取る
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
            // 接続元が不明な場合は自前のマスタープレイリストを返す
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
                    // 接続元ごとの転送先を共有キャッシュが使い回さないようにする
                    (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                ],
            )
                .into_response());
        }
    };
    // 応答は接続元ごとに変わるため、共有キャッシュが使い回さないようにする
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
        .sync_worker(
            &worker_id,
            request.cached_segments,
            request.fetching_segments,
            now_ms(),
        )
        .await?;
    Ok(Json(response))
}

#[cfg(test)]
mod cluster_tests {
    use super::*;
    use clap::Parser;
    use tower::ServiceExt;

    #[test]
    fn keeps_prefix_path_and_query_in_location() {
        let uri = "/segments/live/720p/0.ts?k=a%2Fb&x=1".parse().unwrap();
        assert_eq!(
            leader_location("https://leader.test/base", &uri),
            "https://leader.test/base/segments/live/720p/0.ts?k=a%2Fb&x=1"
        );
        assert_eq!(
            leader_location("http://leader.test", &"/".parse().unwrap()),
            "http://leader.test/"
        );
    }

    #[tokio::test]
    async fn returns_retry_after_until_first_heartbeat_round_completes() {
        let config = crate::ControllerConfig::parse_from([
            "controller",
            "--bind",
            "127.0.0.1:0",
            "--storage-capacity",
            "1MiB",
            "--playlist-segments",
            "6",
            "--sync-interval-ms",
            "100",
            "--sync-timeout-ms",
            "1000",
            "--public-url",
            "http://a.test",
            "--metric",
            "10",
            "--seed",
            "http://b.test",
            "--auth-token",
            "token",
        ]);
        let state = ControllerState::new(crate::CanonicalStore::new(1024), 6, 100, 1000, 0, 1);
        let cluster = ClusterState::new(&config, state.clone()).unwrap();
        // 実際の初期状態を使い、heartbeat を開始する前の応答を確認する。
        let app = router_with_cluster(state.clone(), &CorsOrigins::Any, Some("token"), cluster);
        for (method, path) in [
            (Method::GET, "/segments/live/720p/0.ts"),
            (Method::PUT, "/workers/a"),
            (Method::POST, "/workers/a/sync"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method.clone())
                        .uri(path)
                        .header(header::AUTHORIZATION, "Bearer token")
                        .header(header::ORIGIN, "https://player.test")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            if method == Method::GET {
                assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
            }
        }
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::PUT)
                    .uri("/workers/a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "未確定時も PUT は先に認証する"
        );
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["role"], "follower");
        assert!(health["leader_url"].is_null());
        assert_eq!(state.store.used_bytes(), 0);
        assert!(state.inner.lock().await.workers.is_empty());
    }
}
