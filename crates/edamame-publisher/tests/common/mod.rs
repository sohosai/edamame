//! 結合テストで使う実サーバーと実プロセスの起動と後始末
//!
//! モックは使わず、controller と worker は同じリポジトリの crate としてその場で起動し、
//! ffmpeg と publisher は別プロセスとして起動する。

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use edamame_controller::{CanonicalStore, ControllerState, router as controller_router};
use edamame_domain::CorsOrigins;
use edamame_worker::{Cache, WorkerState, router as worker_router, run_sync_loop};

/// テスト用の空きポートを確保する
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("空きポートを確保できません");
    listener.local_addr().unwrap().port()
}

/// テストごとの一時ディレクトリ
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "edamame-publisher-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("一時ディレクトリを作成できません");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// すべての origin を許可する設定を返す
fn cors() -> CorsOrigins {
    CorsOrigins::parse(&["*".to_owned()]).expect("CORS の設定を作れません")
}

/// HTTP server を別スレッドで起動し、待ち受けている URL を返す
fn serve(app: axum::Router) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime を作れません");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("待ち受けできません");
            sender
                .send(listener.local_addr().expect("アドレスを読めません"))
                .expect("アドレスを渡せません");
            axum::serve(listener, app)
                .await
                .expect("server が止まりました");
        });
    });
    let address = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("server が起動しません");
    format!("http://{address}")
}

/// 条件が成り立つまで待つ
pub fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// controller を起動する
pub struct Controller {
    pub base_url: String,
    pub storage_dir: PathBuf,
}

impl Controller {
    /// テストのプロセスの中で controller を起動する
    pub fn start(dir: &Path, auth_token: &str) -> Self {
        let storage_dir = dir.join("controller");
        fs::create_dir_all(&storage_dir).expect("保存先を作成できません");
        let store = CanonicalStore::new(storage_dir.clone());
        // 公開待ちは 0、同期の間隔は 1 秒にしてテストを短くする
        // 外部配信へ流す割合は controller のテストと同じ 1 % にし、外部配信は設定しない
        let state = ControllerState::new(store, 6, 1000, 10000, 0, 1);
        let base_url = serve(controller_router(state, &cors(), Some(auth_token)));
        Self {
            base_url,
            storage_dir,
        }
    }

    /// `(channel_id, quality)` の保存済みセグメント番号を返す
    pub fn sequences(&self, channel_id: &str, quality: &str) -> Vec<u64> {
        let dir = self.storage_dir.join(channel_id).join(quality);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut sequences = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".ts"))
                    .and_then(|name| name.parse::<u64>().ok())
            })
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        sequences
    }

    /// セグメントファイルのパスを返す
    pub fn segment_path(&self, channel_id: &str, quality: &str, sequence: u64) -> PathBuf {
        self.storage_dir
            .join(channel_id)
            .join(quality)
            .join(format!("{sequence}.ts"))
    }
}

/// worker を起動する
pub struct Worker;

impl Worker {
    /// テストのプロセスの中で worker と sync を起動する
    pub fn start(dir: &Path, controller_url: &str, auth_token: &str) -> Self {
        let port = free_port();
        let cache_dir = dir.join("worker");
        fs::create_dir_all(&cache_dir).expect("キャッシュ先を作成できません");
        let public_url = format!("http://127.0.0.1:{port}");
        let controller_url = controller_url.to_owned();
        let auth_token = auth_token.to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime を作れません");
            runtime.block_on(async move {
                let cache = Cache::scan(cache_dir, 12)
                    .await
                    .expect("キャッシュを読めません");
                let state = WorkerState::new(
                    "worker-test".to_owned(),
                    controller_url,
                    public_url,
                    1,
                    cache,
                )
                .expect("worker の状態を作れません");
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .expect("待ち受けできません");
                sender.send(()).expect("起動を伝えられません");
                let sync_state = Arc::clone(&state);
                tokio::spawn(async move { run_sync_loop(sync_state, Some(auth_token)).await });
                axum::serve(listener, worker_router(state, &cors()))
                    .await
                    .expect("server が止まりました");
            });
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("worker が起動しません");
        Self
    }
}

/// SRT を送る ffmpeg
pub struct Sender {
    child: Child,
}

impl Sender {
    /// testsrc2 と sine を H.264 と AAC か MP2 の MPEG-TS にして送る
    pub fn start(
        port: u16,
        resolution: &str,
        fps: u32,
        gop: u32,
        audio_codec: &str,
        seconds: u32,
    ) -> Self {
        let url = format!("srt://127.0.0.1:{port}?mode=caller&latency=200000");
        for _ in 0..5 {
            let child = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "warning",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc2=size={resolution}:rate={fps}"),
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-t",
                    &seconds.to_string(),
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-g",
                    &gop.to_string(),
                    "-sc_threshold",
                    "0",
                    "-pix_fmt",
                    "yuv420p",
                    "-b:v",
                    "800k",
                    "-c:a",
                    audio_codec,
                    "-b:a",
                    "64k",
                    "-f",
                    "mpegts",
                    &url,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("ffmpeg を起動できません");
            thread::sleep(Duration::from_millis(800));
            let mut child = child;
            if child.try_wait().expect("ffmpeg を確認できません").is_none() {
                return Self { child };
            }
            thread::sleep(Duration::from_millis(500));
        }
        panic!("SRT を送信できません。publisher が待ち受けているか確認してください");
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// publisher を起動してログを集める
pub struct Publisher {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
}

impl Publisher {
    pub fn start(
        config: &Path,
        controller_url: &str,
        auth_token: Option<&str>,
        work_dir: &Path,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_edamame-publisher"));
        command
            .arg("--config")
            .arg(config)
            .arg("--controller-url")
            .arg(controller_url)
            .arg("--work-dir")
            .arg(work_dir);
        if let Some(token) = auth_token {
            command.arg("--auth-token").arg(token);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().expect("publisher を起動できません");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().expect("標準出力を読めません");
        let stderr = child.stderr.take().expect("標準エラー出力を読めません");
        capture(stdout, Arc::clone(&log));
        capture(stderr, Arc::clone(&log));
        Self { child, log }
    }

    /// 標準出力と標準エラー出力に書かれた内容を返す
    pub fn log(&self) -> String {
        self.log.lock().expect("ログを読めません").join("\n")
    }

    /// 起動時に表示した鍵を返す
    pub fn key(&self) -> String {
        self.log()
            .lines()
            .find_map(|line| {
                line.strip_prefix("encryption key (hex) is ")
                    .map(str::to_owned)
            })
            .expect("publisher が鍵を表示していません")
    }

    pub fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        wait_for(|| self.log().contains(needle), timeout)
    }

    /// SIGTERM を送って終了を待つ
    pub fn stop(&mut self) -> ExitStatus {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("publisher を確認できません")
            {
                return status;
            }
            assert!(Instant::now() < deadline, "publisher が終了しません");
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// 実行中かどうか
    pub fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("publisher を確認できません")
            .is_none()
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 子プロセスの出力を 1 行ずつ集める
fn capture(reader: impl std::io::Read + Send + 'static, log: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            log.lock().expect("ログを書けません").push(line);
        }
    });
}

/// HTTP で取得する。失敗した場合は None を返す
pub fn http_get(url: &str) -> Option<String> {
    let output = Command::new("curl")
        .args(["-s", "-f", "--max-time", "5", url])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// hex 文字列をバイト列にする
pub fn hex_decode(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| {
            u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex を読めません")
        })
        .collect()
}

/// 暗号化されたセグメントを、プレイリストに書かれた IV で復号する
pub fn decrypt_segment(key_hex: &str, iv_hex: &str, data: &[u8]) -> Vec<u8> {
    let key = hex_decode(key_hex);
    let iv = hex_decode(iv_hex);
    cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, &iv)
        .expect("鍵の長さが違います")
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .expect("復号できません")
}

/// MPEG-TS として並んでいるか確かめる
pub fn assert_mpegts(data: &[u8]) {
    assert!(data.len() >= 188 * 2, "セグメントが短すぎます");
    assert_eq!(data[0], 0x47, "先頭が MPEG-TS の同期バイトではありません");
    assert_eq!(
        data[188], 0x47,
        "188 バイト目が MPEG-TS の同期バイトではありません"
    );
}
