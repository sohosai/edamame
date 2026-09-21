//! worker の HTTP 配信を実サーバーで確認する

use edamame_domain::CorsOrigins;
use edamame_worker::{Cache, WorkerState, router};

/// テストで使うセグメントを入れたキャッシュを作る
fn cache_with(segments: &[(&str, &str, u64, &[u8])]) -> Cache {
    let mut cache = Cache::new(12).unwrap();
    for (channel_id, quality, sequence, bytes) in segments {
        cache
            .put(channel_id, quality, *sequence, bytes.to_vec())
            .unwrap();
    }
    cache
}

#[tokio::test]
async fn serves_segments_under_public_url_path_prefix() {
    let cache = cache_with(&[("live", "720p", 101, b"segment-101")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = WorkerState::new(
        "worker-a".to_owned(),
        "http://127.0.0.1:9000".to_owned(),
        "http://127.0.0.1:9001/base".to_owned(),
        1,
        cache,
    )
    .unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/base/segments/live/720p/101.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"segment-101");
    let root = client
        .get(format!("http://{addr}/segments/live/720p/101.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(root.status().as_u16(), 404);
}

#[tokio::test]
async fn missing_segment_returns_404() {
    let cache = cache_with(&[("live", "720p", 101, b"segment-101")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = WorkerState::new(
        "worker-a".to_owned(),
        "http://127.0.0.1:9000".to_owned(),
        format!("http://{addr}"),
        1,
        cache,
    )
    .unwrap();
    let app = router(state, &CorsOrigins::Disabled);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/segments/live/720p/102.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

/// パスの接頭辞にある axum 7 の記法に似た文字も、URL の一部として扱う
#[tokio::test]
async fn serves_segments_under_path_prefix_with_route_characters() {
    for path in ["/foo/:tenant", "/foo/*", "/foo{bar}"] {
        let cache = cache_with(&[("live", "720p", 101, b"segment-101")]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = WorkerState::new(
            "worker-a".to_owned(),
            "http://127.0.0.1:9000".to_owned(),
            format!("http://{addr}{path}"),
            1,
            cache,
        )
        .unwrap();
        let app = router(state, &CorsOrigins::Disabled);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // リクエストの URL に含めるため、`{` と `}` を `%7B` と `%7D` に変換する
        let request_path = path.replace('{', "%7B").replace('}', "%7D");
        let response = reqwest::Client::new()
            .get(format!(
                "http://{addr}{request_path}/segments/live/720p/101.ts"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "path prefix: {path}");
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"segment-101");
    }
}
