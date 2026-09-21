//! バイナリを起動し、環境変数からの設定を確認する

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn reads_configuration_from_environment() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_edamame-worker"))
        .env("EDAMAME_WORKER_ID", "worker-env")
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_PUBLIC_URL", "http://127.0.0.1:0")
        .env("EDAMAME_CONTROLLER_URL", "http://127.0.0.1:1")
        // CLI の値が優先されることを確かめるため、EDAMAME_CACHE_LIMIT に不正な値を設定する
        .env("EDAMAME_CACHE_LIMIT", "0")
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
}

/// 空文字列は未指定とは異なり、CORS の設定値として検証される
#[tokio::test]
async fn rejects_empty_environment_variable() {
    let child = Command::new(env!("CARGO_BIN_EXE_edamame-worker"))
        .env("EDAMAME_WORKER_ID", "worker-env")
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_PUBLIC_URL", "http://127.0.0.1:0")
        .env("EDAMAME_CONTROLLER_URL", "http://127.0.0.1:1")
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
}
