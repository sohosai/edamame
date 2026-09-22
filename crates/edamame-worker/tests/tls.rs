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
    let config = edamame_worker::tls::static_config(cert, key).await.unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    tokio::spawn(async move {
        edamame_worker::tls::serve_static(listener, app, config)
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
