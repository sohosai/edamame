//! 環境変数からの設定を実バイナリで確認する

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "edamame-worker-config-env-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// 環境変数だけで起動でき、CLI の値は環境変数より優先される
#[tokio::test]
async fn reads_configuration_from_environment() {
    let dir = temp_dir("env");
    let mut child = Command::new(env!("CARGO_BIN_EXE_edamame-worker"))
        .env("EDAMAME_WORKER_ID", "worker-env")
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_PUBLIC_URL", "http://127.0.0.1:0")
        .env("EDAMAME_CONTROLLER_URL", "http://127.0.0.1:1")
        .env("EDAMAME_CACHE_DIR", dir.join("cache"))
        // 環境変数の値は不正だが、CLI の値が優先される
        .env("EDAMAME_CACHE_LIMIT", "0")
        // CORS は未指定にする
        .env_remove("EDAMAME_CORS_ORIGIN")
        .args(["--cache-limit", "12"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut stderr_lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let warning = tokio::time::timeout(Duration::from_secs(10), stderr_lines.next_line())
        .await
        .expect("警告の出力がタイムアウトした")
        .unwrap()
        .expect("警告が出力されなかった");
    assert!(warning.contains("cors-origin"), "想定と違う警告: {warning}");

    let mut stdout_lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = tokio::time::timeout(Duration::from_secs(10), stdout_lines.next_line())
        .await
        .expect("起動の出力がタイムアウトした")
        .unwrap()
        .expect("起動のログが出力されなかった");
    assert!(
        listening.contains("edamame-worker worker-env listening on"),
        "想定と違うログ: {listening}"
    );

    child.kill().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// 空の環境変数は値として扱うため、起動に失敗する
#[tokio::test]
async fn rejects_empty_environment_variable() {
    let dir = temp_dir("env-empty");
    let child = Command::new(env!("CARGO_BIN_EXE_edamame-worker"))
        .env("EDAMAME_WORKER_ID", "worker-env")
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_PUBLIC_URL", "http://127.0.0.1:0")
        .env("EDAMAME_CONTROLLER_URL", "http://127.0.0.1:1")
        .env("EDAMAME_CACHE_DIR", dir.join("cache"))
        .env("EDAMAME_CACHE_LIMIT", "12")
        .env("EDAMAME_CORS_ORIGIN", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("終了がタイムアウトした")
        .unwrap();
    assert!(!output.status.success(), "起動に成功してしまった");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cors origin is invalid"),
        "想定と違うエラー: {stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
