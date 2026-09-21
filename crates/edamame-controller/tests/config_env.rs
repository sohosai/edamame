//! controller を起動し、環境変数から設定を読み込めることを確認する

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// 環境変数だけで起動でき、CLI の値は環境変数より優先される
#[tokio::test]
async fn reads_configuration_from_environment() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_edamame-controller"))
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_STORAGE_CAPACITY", "256MiB")
        .env("EDAMAME_SYNC_INTERVAL_MS", "100")
        .env("EDAMAME_SYNC_TIMEOUT_MS", "1000")
        .env("EDAMAME_PUBLISH_WAIT_MS", "2000")
        .env("EDAMAME_AUTH_TOKEN", "example-token")
        // 環境変数の値は不正だが、CLI の値が優先される
        .env("EDAMAME_PLAYLIST_SEGMENTS", "5")
        // CORS は未指定にする
        .env_remove("EDAMAME_CORS_ORIGIN")
        .args(["--playlist-segments", "6"])
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
        listening.contains("listening on"),
        "想定と違うログ: {listening}"
    );

    child.kill().await.unwrap();
}

/// EDAMAME_CORS_ORIGIN に空文字列を指定すると、不正な値として起動に失敗する
#[tokio::test]
async fn rejects_empty_environment_variable() {
    let child = Command::new(env!("CARGO_BIN_EXE_edamame-controller"))
        .env("EDAMAME_BIND", "127.0.0.1:0")
        .env("EDAMAME_STORAGE_CAPACITY", "256MiB")
        .env("EDAMAME_PLAYLIST_SEGMENTS", "6")
        .env("EDAMAME_SYNC_INTERVAL_MS", "100")
        .env("EDAMAME_SYNC_TIMEOUT_MS", "1000")
        .env("EDAMAME_PUBLISH_WAIT_MS", "2000")
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
