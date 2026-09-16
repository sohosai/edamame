//! HTTP テストで使う一時ディレクトリと実サーバー

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use edamame_controller::storage::CanonicalStore;
use edamame_controller::{ControllerState, router as controller_router};
use edamame_domain::CorsOrigins;
use edamame_worker::{Cache, WorkerState, router as worker_router};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

static COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// controller を実際の HTTP サーバーとして起動する
pub async fn spawn_controller(
    addr: SocketAddr,
    storage_dir: PathBuf,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_dir,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            cors: CorsOrigins::Disabled,
            auth_token: None,
        },
    )
    .await
}

/// 公開待ち時間を指定して controller を実際の HTTP サーバーとして起動する
pub async fn spawn_controller_with_publish_wait(
    addr: SocketAddr,
    storage_dir: PathBuf,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    publish_wait_ms: u64,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_dir,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms,
            cors: CorsOrigins::Disabled,
            auth_token: None,
        },
    )
    .await
}

/// CORS の設定を指定して controller を実際の HTTP サーバーとして起動する
pub async fn spawn_controller_with_cors(
    addr: SocketAddr,
    storage_dir: PathBuf,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    cors: CorsOrigins,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_dir,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            cors,
            auth_token: None,
        },
    )
    .await
}

/// 認証トークンを指定して controller を実際の HTTP サーバーとして起動する
pub async fn spawn_controller_with_auth(
    addr: SocketAddr,
    storage_dir: PathBuf,
    playlist_segments: u64,
    sync_interval_ms: u64,
    sync_timeout_ms: u64,
    auth_token: &str,
) -> ControllerHandle {
    spawn_controller_with_options(
        addr,
        storage_dir,
        ControllerOptions {
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms: 0,
            cors: CorsOrigins::Disabled,
            auth_token: Some(auth_token),
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
    cors: CorsOrigins,
    auth_token: Option<&'a str>,
}

/// controller の全設定を指定して実際の HTTP サーバーとして起動する
async fn spawn_controller_with_options(
    addr: SocketAddr,
    storage_dir: PathBuf,
    options: ControllerOptions<'_>,
) -> ControllerHandle {
    tokio::fs::create_dir_all(&storage_dir).await.unwrap();
    let store = CanonicalStore::new(storage_dir);
    let state = ControllerState::new(
        store,
        options.playlist_segments,
        options.sync_interval_ms,
        options.sync_timeout_ms,
        options.publish_wait_ms,
    );
    let listener = TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = controller_router(state.clone(), &options.cors, options.auth_token);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    tokio::spawn(async move {
        axum::serve(listener, app)
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
    cache_dir: PathBuf,
    cache_limit: u64,
    start_sync: bool,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_dir,
        cache_limit,
        start_sync,
        WorkerOptions::default(),
    )
    .await
}

/// CORS の設定を指定して worker を実際の HTTP サーバーとして起動する
pub async fn spawn_worker_with_cors(
    worker_id: &str,
    controller_url: &str,
    cache_dir: PathBuf,
    cache_limit: u64,
    start_sync: bool,
    cors: CorsOrigins,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_dir,
        cache_limit,
        start_sync,
        WorkerOptions {
            cors,
            ..WorkerOptions::default()
        },
    )
    .await
}

/// 認証トークンを指定して worker を実際の HTTP サーバーとして起動する
pub async fn spawn_worker_with_auth(
    worker_id: &str,
    controller_url: &str,
    cache_dir: PathBuf,
    cache_limit: u64,
    start_sync: bool,
    auth_token: &str,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_dir,
        cache_limit,
        start_sync,
        WorkerOptions {
            auth_token: Some(auth_token.to_owned()),
            ..WorkerOptions::default()
        },
    )
    .await
}

/// 重みを指定して worker を実際の HTTP サーバーとして起動する
pub async fn spawn_worker_with_weight(
    worker_id: &str,
    controller_url: &str,
    cache_dir: PathBuf,
    cache_limit: u64,
    start_sync: bool,
    weight: u32,
) -> WorkerHandle {
    spawn_worker_with_options(
        worker_id,
        controller_url,
        cache_dir,
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
            auth_token: None,
            weight: 1,
        }
    }
}

async fn spawn_worker_with_options(
    worker_id: &str,
    controller_url: &str,
    cache_dir: PathBuf,
    cache_limit: u64,
    start_sync: bool,
    options: WorkerOptions,
) -> WorkerHandle {
    let WorkerOptions {
        cors,
        auth_token,
        weight,
    } = options;
    tokio::fs::create_dir_all(&cache_dir).await.unwrap();
    let cache = Cache::scan(cache_dir, cache_limit).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let public_url = format!("http://{addr}");
    let state = WorkerState::new(
        worker_id.to_owned(),
        controller_url.to_owned(),
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

/// playlist が 200 になるまで待つ
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

/// playlist が 200 になり、指定した文字列を含むまで待つ
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
