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
    spawn_controller_with_cors(
        addr,
        storage_dir,
        playlist_segments,
        sync_interval_ms,
        sync_timeout_ms,
        CorsOrigins::Disabled,
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
    tokio::fs::create_dir_all(&storage_dir).await.unwrap();
    let store = CanonicalStore::new(storage_dir);
    let state = ControllerState::new(store, playlist_segments, sync_interval_ms, sync_timeout_ms);
    let listener = TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = controller_router(state.clone(), &cors);
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
    spawn_worker_with_cors(
        worker_id,
        controller_url,
        cache_dir,
        cache_limit,
        start_sync,
        CorsOrigins::Disabled,
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
    tokio::fs::create_dir_all(&cache_dir).await.unwrap();
    let cache = Cache::scan(cache_dir, cache_limit).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let public_url = format!("http://{addr}");
    let state = WorkerState::new(
        worker_id.to_owned(),
        controller_url.to_owned(),
        public_url.clone(),
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
            edamame_worker::run_sync_loop(sync_state).await;
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
