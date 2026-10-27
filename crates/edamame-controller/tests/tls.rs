//! controller の TLS 配信を自己署名証明書で確認する

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use edamame_controller::cluster::ClusterState;
use edamame_controller::config::ControllerConfig;
use edamame_controller::http::router;
use edamame_controller::state::ControllerState;
use edamame_controller::storage::CanonicalStore;
use edamame_domain::CorsOrigins;
use tokio::process::Command;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "edamame-controller-tls-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn serves_health_over_static_tls() {
    let dir = temp_dir("static");
    // 自己署名証明書を作る
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    // TLS の待ち受けに使う証明書とは別に、cluster の設定は https の公開 URL だけを渡す
    let config = ControllerConfig {
        bind: addr,
        storage_capacity: 16 * 1024 * 1024,
        playlist_segments: 6,
        sync_interval_ms: 100,
        sync_timeout_ms: 1000,
        publish_wait_ms: Some(0),
        auth_token: "example-token".to_owned(),
        cors_origin: vec![],
        public_url: "https://localhost".to_owned(),
        seed: vec![],
        tls_cert: None,
        tls_key: None,
        acme: false,
        acme_directory: None,
        acme_cache_dir: None,
        acme_contact: None,
        external_min_percent: 1,
    };
    let state = ControllerState::new(CanonicalStore::new(16 * 1024 * 1024), 6, 100, 1000, 0, 1);
    let cluster = ClusterState::new(&config, state.clone()).unwrap();
    let app = router(state, &CorsOrigins::Disabled, cluster);
    let tls_config = edamame_tls::static_config(cert, key).await.unwrap();
    tokio::spawn(async move {
        edamame_tls::serve_static(listener, app, tls_config)
            .await
            .unwrap();
    });

    // 自己署名証明書を受け入れて取得する
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/health", addr.port());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client.get(&url).send().await {
            Ok(response) => {
                assert_eq!(response.status().as_u16(), 200);
                assert_eq!(response.text().await.unwrap(), "ok");
                break;
            }
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("TLS で health を取得できません: {error}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn serves_health_from_binary_over_static_tls() {
    let dir = temp_dir("binary");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    // ポートを予約してから手放し、実プロセスに同じアドレスを渡す
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_edamame-controller"));
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("EDAMAME_")) {
        command.env_remove(name);
    }
    command
        .args([
            "--bind",
            &addr.to_string(),
            "--public-url",
            &format!("https://localhost:{}", addr.port()),
            "--storage-capacity",
            "1MiB",
            "--playlist-segments",
            "6",
            "--sync-interval-ms",
            "100",
            "--sync-timeout-ms",
            "1000",
            "--publish-wait-ms",
            "0",
            "--auth-token",
            "example-token",
            "--cors-origin",
            "none",
            "--tls-cert",
        ])
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/health", addr.port());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match client.get(&url).send().await {
            Ok(response) => {
                assert_eq!(response.status().as_u16(), 200);
                assert_eq!(response.text().await.unwrap(), "ok");
                break;
            }
            Err(error) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("controller が終了しました: {status}");
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("TLS で health を取得できません: {error}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    child.kill().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
