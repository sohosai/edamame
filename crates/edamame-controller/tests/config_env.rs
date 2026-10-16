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
        .env("EDAMAME_PUBLIC_URL", "https://controller.test/base/")
        .env("EDAMAME_METRIC", "20")
        .env("EDAMAME_SEED", "http://peer.test,http://peer.test/")
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

    let cluster = tokio::time::timeout(Duration::from_secs(10), stdout_lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        cluster.contains("https://controller.test/base"),
        "公開 URL が正規化されなかった: {cluster}"
    );
    assert!(
        cluster.contains("metric=20"),
        "環境変数の metric が反映されなかった: {cluster}"
    );
    assert!(
        cluster.contains("role=Follower"),
        "起動直後に leader を名乗った: {cluster}"
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

/// seed を指定しても token がなければ警告を出して単独 leader になる。
#[tokio::test]
async fn disables_cluster_with_warning_without_auth_token() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let base = format!("http://{addr}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_edamame-controller"))
        .env("EDAMAME_BIND", addr.to_string())
        .env("EDAMAME_STORAGE_CAPACITY", "1MiB")
        .env("EDAMAME_PLAYLIST_SEGMENTS", "6")
        .env("EDAMAME_SYNC_INTERVAL_MS", "100")
        .env("EDAMAME_SYNC_TIMEOUT_MS", "1000")
        .env("EDAMAME_PUBLISH_WAIT_MS", "0")
        .env("EDAMAME_CORS_ORIGIN", "none")
        .env("EDAMAME_PUBLIC_URL", &base)
        .env("EDAMAME_METRIC", "42")
        .env("EDAMAME_SEED", "http://127.0.0.1:1")
        .env_remove("EDAMAME_AUTH_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let warning = tokio::time::timeout(Duration::from_secs(10), stderr.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        warning.contains("cluster is disabled") && warning.contains("--auth-token"),
        "無効化の警告が出なかった: {warning}"
    );
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let client = reqwest::Client::new();
    let health: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["role"], "leader");
    assert_eq!(health["leader_url"], base);
    assert_eq!(health["metric"], 42);
    let response = client
        .post(format!("{base}/internal/cluster/heartbeat"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    child.kill().await.unwrap();
}
