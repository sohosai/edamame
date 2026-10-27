//! worker の TLS 配信を自己署名証明書で確認する

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use edamame_domain::CorsOrigins;
use edamame_worker::cache::Cache;
use edamame_worker::http::{WorkerState, router};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "edamame-worker-tls-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn serves_segments_over_static_tls() {
    let dir = temp_dir("static");
    // 自己署名証明書を作る
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    // セグメントを 1 つ持つ worker を TLS で起動する
    let mut cache = Cache::new(12).unwrap();
    cache
        .put("live", "720p", 101, Bytes::from_static(b"segment-101"))
        .unwrap();
    let state = WorkerState::new(
        "worker-a".to_owned(),
        "http://127.0.0.1:9000".to_owned(),
        "https://localhost".to_owned(),
        1,
        cache,
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = edamame_tls::static_config(cert, key).await.unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    tokio::spawn(async move {
        edamame_tls::serve_static(listener, app, config)
            .await
            .unwrap();
    });

    // 起動を待ってから、自己署名証明書を受け入れて取得する
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://{addr}/segments/live/720p/101.ts");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client.get(&url).send().await {
            Ok(response) => {
                assert_eq!(response.status().as_u16(), 200);
                assert_eq!(response.bytes().await.unwrap().as_ref(), b"segment-101");
                break;
            }
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("TLS でセグメントを取得できません: {error}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn static_server_stops_when_shutdown_fires() {
    let dir = temp_dir("shutdown");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    let state = WorkerState::new(
        "worker-a".to_owned(),
        "http://127.0.0.1:9000".to_owned(),
        "https://localhost".to_owned(),
        1,
        Cache::new(12).unwrap(),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = edamame_tls::static_config(cert, key).await.unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    let (shutdown, wait_shutdown) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        edamame_tls::serve_static_until(listener, app, config, async {
            let _ = wait_shutdown.await;
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("TLS サーバーが終了しませんでした")
        .expect("TLS サーバーのタスクが panic しました");
    assert!(
        result.is_ok(),
        "TLS サーバーがエラーで終了しました: {result:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn resumes_previous_session() {
    let dir = temp_dir("resumption");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

    let mut cache = Cache::new(12).unwrap();
    cache
        .put("live", "720p", 101, Bytes::from_static(b"segment-101"))
        .unwrap();
    let state = WorkerState::new(
        "worker-a".to_owned(),
        "http://127.0.0.1:9000".to_owned(),
        "https://localhost".to_owned(),
        1,
        cache,
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = edamame_tls::static_config(cert, key).await.unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    tokio::spawn(async move {
        edamame_tls::serve_static(listener, app, config)
            .await
            .unwrap();
    });

    // 再開を使う client で 2 回接続し、2 回目がハンドシェイクを省くことを確かめる
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let client_config = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let connector = tokio_rustls::TlsConnector::from(client_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut kinds = Vec::new();
    for attempt in 1..=2 {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut tls = connector
            .connect(server_name.clone(), stream)
            .await
            .unwrap_or_else(|error| panic!("{attempt} 回目の接続に失敗しました: {error}"));
        // 応答を最後まで読んで、セッションが確定してから閉じる
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tls.write_all(b"GET /segments/live/720p/101.ts HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        tls.read_to_end(&mut body).await.unwrap();
        assert!(
            body.starts_with(b"HTTP/1.1 200"),
            "{attempt} 回目の応答: {:?}",
            String::from_utf8_lossy(&body[..40.min(body.len())])
        );
        kinds.push(tls.get_ref().1.handshake_kind());
    }
    // 1 回目は完全なハンドシェイク、2 回目は前回のセッションを再開する
    assert_eq!(kinds[0], Some(rustls::HandshakeKind::Full));
    assert_eq!(
        kinds[1],
        Some(rustls::HandshakeKind::Resumed),
        "2 回目の接続でセッションを再開できませんでした"
    );
    let _ = std::fs::remove_dir_all(dir);
}
