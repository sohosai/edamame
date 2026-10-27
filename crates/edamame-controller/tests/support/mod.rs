//! HTTP テストで使う一時ディレクトリと実サーバー

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use edamame_controller::cluster::ClusterState;
use edamame_controller::storage::CanonicalStore;
use edamame_controller::{ControllerConfig, ControllerState, router as controller_router};
use edamame_domain::CorsOrigins;
use edamame_worker::{Cache, WorkerState, router as worker_router};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub const AUTH_TOKEN: &str = "example-token";

/// テストで使う既定の保存容量。セグメントの内容より十分に大きくする
pub const DEFAULT_STORAGE_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;

/// テスト終了時に削除される一時ディレクトリ
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "edamame-{tag}-{}-{nanos}-{counter}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 実行中の controller
pub struct ControllerHandle {
    pub addr: SocketAddr,
    pub base_url: String,
    pub state: Arc<ControllerState>,
    shutdown: Option<oneshot::Sender<()>>,
    heartbeat_task: tokio::task::JoinHandle<()>,
}

impl ControllerHandle {
    /// サーバーを停止し、同じアドレスを再利用できるようにする
    pub async fn shutdown(mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        // サーバーと同じ寿命にし、停止後も配信状態を保持するタスクを残さない。
        self.heartbeat_task.abort();
    }
}

/// controller を実際の HTTP サーバーとして起動する
pub async fn spawn_controller(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_capacity_bytes,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            external_min_percent: 1,
            cors: CorsOrigins::Disabled,
            auth_token: AUTH_TOKEN,
        },
    )
    .await
}

/// 公開待ちを指定して controller の HTTP サーバーを起動する
pub async fn spawn_controller_with_publish_wait(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    publish_wait_ms: u64,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_capacity_bytes,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms,
            external_min_percent: 1,
            cors: CorsOrigins::Disabled,
            auth_token: AUTH_TOKEN,
        },
    )
    .await
}

/// CORS の設定を指定して controller の HTTP サーバーを起動する
pub async fn spawn_controller_with_cors(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    cors: CorsOrigins,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_capacity_bytes,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            external_min_percent: 1,
            cors,
            auth_token: AUTH_TOKEN,
        },
    )
    .await
}

/// 認証トークンを指定して controller の HTTP サーバーを起動する
pub async fn spawn_controller_with_auth(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    auth_token: &str,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_capacity_bytes,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            external_min_percent: 1,
            cors: CorsOrigins::Disabled,
            auth_token,
        },
    )
    .await
}

/// 外部配信へ振り分ける最低の割合と公開待ちを指定して controller を起動する
pub async fn spawn_controller_with_external_percent(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    publish_wait_ms: u64,
    external_min_percent: u8,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_capacity_bytes,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms,
            external_min_percent,
            cors: CorsOrigins::Disabled,
            auth_token: AUTH_TOKEN,
        },
    )
    .await
}

/// controller の起動オプション
struct ControllerOptions<'a> {
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    publish_wait_ms: u64,
    external_min_percent: u8,
    cors: CorsOrigins,
    auth_token: &'a str,
}

/// controller の全設定を指定して HTTP サーバーを起動する
async fn spawn_controller_with_options(
    addr: SocketAddr,
    storage_capacity_bytes: u64,
    options: ControllerOptions<'_>,
) -> ControllerHandle {
    let store = CanonicalStore::new(storage_capacity_bytes);
    let state = ControllerState::new(
        store,
        options.playlist_segments,
        options.sync_interval_ms,
        options.sync_timeout_ms,
        options.publish_wait_ms,
        options.external_min_percent,
    );
    let listener = TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ControllerConfig {
        bind: addr,
        storage_capacity: storage_capacity_bytes,
        playlist_segments: options.playlist_segments,
        sync_interval_ms: options.sync_interval_ms,
        sync_timeout_ms: options.sync_timeout_ms,
        publish_wait_ms: Some(options.publish_wait_ms),
        external_min_percent: options.external_min_percent,
        cors_origin: vec![],
        public_url: format!("http://{addr}"),
        auth_token: options.auth_token.to_owned(),
        seed: vec![],
        tls_cert: None,
        tls_key: None,
        acme: false,
        acme_directory: None,
        acme_cache_dir: None,
        acme_contact: None,
    };
    let cluster = ClusterState::new(&config, state.clone()).unwrap();
    let heartbeat_task = tokio::spawn(cluster.clone().run());
    let app = controller_router(state.clone(), &options.cors, cluster);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    tokio::spawn(async move {
        // 外部配信への振り分けで接続元を使うため、接続情報を付けて配信する
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_receiver.await;
        })
        .await
        .unwrap();
    });
    ControllerHandle {
        addr,
        base_url: format!("http://{addr}"),
        state,
        shutdown: Some(shutdown_sender),
        heartbeat_task,
    }
}

/// 実行中の worker
pub struct WorkerHandle {
    pub base_url: String,
    pub state: Arc<WorkerState>,
}

/// worker を実際の HTTP サーバーとして起動する
pub async fn spawn_worker(
    worker_id: &str,
    controller_url: &str,
    cache_limit: u64,
    start_sync: bool,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_limit,
        start_sync,
        WorkerOptions::default(),
    )
    .await
}

/// CORS の設定を指定して worker の HTTP サーバーを起動する
pub async fn spawn_worker_with_cors(
    worker_id: &str,
    controller_url: &str,
    cache_limit: u64,
    start_sync: bool,
    cors: CorsOrigins,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_limit,
        start_sync,
        WorkerOptions {
            cors,
            ..WorkerOptions::default()
        },
    )
    .await
}

/// 認証トークンを指定して worker の HTTP サーバーを起動する
pub async fn spawn_worker_with_auth(
    worker_id: &str,
    controller_url: &str,
    cache_limit: u64,
    start_sync: bool,
    auth_token: &str,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_limit,
        start_sync,
        WorkerOptions {
            auth_token: Some(auth_token.to_owned()),
            ..WorkerOptions::default()
        },
    )
    .await
}

/// 重みを指定して worker の HTTP サーバーを起動する
pub async fn spawn_worker_with_weight(
    worker_id: &str,
    controller_url: &str,
    cache_limit: u64,
    start_sync: bool,
    weight: u32,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_limit,
        start_sync,
        WorkerOptions {
            weight,
            ..WorkerOptions::default()
        },
    )
    .await
}

/// worker を起動するときの追加設定
struct WorkerOptions {
    cors: CorsOrigins,
    auth_token: Option<String>,
    weight: u32,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            cors: CorsOrigins::Disabled,
            auth_token: Some(AUTH_TOKEN.to_owned()),
            weight: 1,
        }
    }
}

async fn spawn_worker_with_options(
    worker_id: &str,
    controller_url: &str,
    cache_limit: u64,
    start_sync: bool,
    options: WorkerOptions,
) -> WorkerHandle {
    spawn_worker_with_controllers_options(
        worker_id,
        vec![controller_url.to_owned()],
        cache_limit,
        start_sync,
        options,
    )
    .await
}

/// 複数の controller を接続先に持つ worker を起動する
pub async fn spawn_worker_with_auth_and_controllers(
    worker_id: &str,
    controller_urls: Vec<String>,
    cache_limit: u64,
    start_sync: bool,
    auth_token: &str,
) -> WorkerHandle {
    let options = WorkerOptions {
        auth_token: Some(auth_token.to_owned()),
        ..WorkerOptions::default()
    };
    spawn_worker_with_controllers_options(
        worker_id,
        controller_urls,
        cache_limit,
        start_sync,
        options,
    )
    .await
}

async fn spawn_worker_with_controllers_options(
    worker_id: &str,
    controller_urls: Vec<String>,
    cache_limit: u64,
    start_sync: bool,
    options: WorkerOptions,
) -> WorkerHandle {
    let WorkerOptions {
        cors,
        auth_token,
        weight,
    } = options;
    let cache = Cache::new(cache_limit).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let public_url = format!("http://{addr}");
    let state = WorkerState::new(
        worker_id.to_owned(),
        controller_urls,
        public_url.clone(),
        weight,
        cache,
    )
    .unwrap();
    let app = worker_router(state.clone(), &cors);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    if start_sync {
        let sync_state = state.clone();
        tokio::spawn(async move {
            edamame_worker::run_sync_loop(sync_state, auth_token).await;
        });
    }
    WorkerHandle {
        base_url: public_url,
        state,
    }
}

/// プレイリストの取得が成功（2xx）するまで待つ
pub async fn wait_for_playlist(client: &reqwest::Client, url: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = client.get(url).send().await.unwrap();
        if response.status().is_success() {
            return response.text().await.unwrap();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("playlist did not become available: {}", response.status());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// プレイリストの取得が成功（2xx）し、本文に指定した文字列が含まれるまで待つ
pub async fn wait_for_playlist_contains(
    client: &reqwest::Client,
    url: &str,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = client.get(url).send().await.unwrap();
        if response.status().is_success() {
            let body = response.text().await.unwrap();
            if body.contains(expected) {
                return body;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("playlist did not contain {expected}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// セグメントファイルが期待するバイト列で取得できるまで待つ
pub async fn wait_for_segment(client: &reqwest::Client, url: &str, expected: &[u8]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = client.get(url).send().await.unwrap();
        if response.status().is_success() {
            let bytes = response.bytes().await.unwrap();
            if bytes.as_ref() == expected {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("セグメントファイルが期待する内容になりません: {url}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 停止と同じアドレスでの再起動を試せるよう、実プロセスと接続先を保持する。
pub struct ControllerProcess {
    pub child: tokio::process::Child,
    pub addr: SocketAddr,
    pub base_url: String,
}

/// 起動前の seed 指定に使うアドレスを返すが、ポートの予約は維持しない。
pub fn available_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// 外部の EDAMAME_ 設定を引き継がず実プロセスを起動し、health の応答まで待つ。
pub async fn spawn_cluster_process(
    addr: SocketAddr,
    seeds: &[String],
    prefix: &str,
) -> ControllerProcess {
    let base_url = format!("http://{addr}{prefix}");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_edamame-controller"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("EDAMAME_")) {
        command.env_remove(name);
    }
    command.args([
        "--bind",
        &addr.to_string(),
        "--storage-capacity",
        "16MiB",
        "--playlist-segments",
        "6",
        "--sync-interval-ms",
        "100",
        "--sync-timeout-ms",
        "1000",
        "--publish-wait-ms",
        "0",
        "--auth-token",
        "cluster-test-token",
        "--cors-origin",
        "*",
        "--public-url",
        &base_url,
    ]);
    if !seeds.is_empty() {
        command.args(["--seed", &seeds.join(",")]);
    }
    let child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let process = ControllerProcess {
        child,
        addr,
        base_url,
    };
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("{}/health", process.base_url))
            .send()
            .await
            .is_ok()
        {
            return process;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "controller が起動しなかった"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// peer の発見が終わるまで health の members を待つ。
pub async fn wait_for_member(client: &reqwest::Client, base_url: &str, member_url: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let health: serde_json::Value = client
            .get(format!("{base_url}/internal/cluster/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let found = health["members"]
            .as_array()
            .is_some_and(|members| members.iter().any(|member| member == member_url));
        if found {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peer を発見できなかった: {health}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
