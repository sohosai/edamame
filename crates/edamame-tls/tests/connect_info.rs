//! TLS 配信でも接続元のアドレスをハンドラーへ渡せることを確認する

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::ConnectInfo;
use axum::routing::get;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("edamame-tls-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn passes_connect_info_over_static_tls() {
    let dir = temp_dir("connect-info");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    let app =
        Router::new().route(
            "/",
            get(
                |ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>| async move {
                    addr.ip().to_string()
                },
            ),
        );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = edamame_tls::static_config(cert, key).await.unwrap();
    tokio::spawn(async move {
        edamame_tls::serve_static(listener, app, config)
            .await
            .unwrap();
    });

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/", addr.port());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client.get(&url).send().await {
            Ok(response) => {
                assert_eq!(response.status().as_u16(), 200);
                assert_eq!(response.text().await.unwrap(), "127.0.0.1");
                break;
            }
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("TLS で接続情報を取得できません: {error}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
