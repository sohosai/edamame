//! worker の HTTP 配信を実サーバーで確認する

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use edamame_domain::CorsOrigins;
use edamame_worker::{Cache, WorkerState, router};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "edamame-worker-http-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_segment(dir: &Path, channel_id: &str, quality: &str, sequence: u64, bytes: &[u8]) {
    let path = dir.join(channel_id).join(quality);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(format!("{sequence}.ts")), bytes).unwrap();
}

#[tokio::test]
async fn serves_segments_under_public_url_path_prefix() {
    let dir = temp_dir("prefix");
    write_segment(&dir, "live", "720p", 101, b"segment-101");
    let cache = Cache::scan(dir.clone(), 12).await.unwrap();
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
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn missing_file_removes_index_entry() {
    let dir = temp_dir("missing");
    write_segment(&dir, "live", "720p", 101, b"segment-101");
    let cache = Cache::scan(dir.clone(), 12).await.unwrap();
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
    let app = router(state.clone(), &CorsOrigins::Disabled);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    std::fs::remove_file(dir.join("live/720p/101.ts")).unwrap();
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/segments/live/720p/101.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let cache = state.cache.lock().await;
    assert!(!cache.contains("live", "720p", 101));
    let _ = std::fs::remove_dir_all(dir);
}

/// 読み取りに失敗したセグメントファイルは索引から外し、次の sync で取得対象に戻す
#[tokio::test]
async fn unreadable_file_removes_index_entry() {
    let dir = temp_dir("unreadable");
    write_segment(&dir, "live", "720p", 101, b"segment-101");
    let cache = Cache::scan(dir.clone(), 12).await.unwrap();
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
    let app = router(state.clone(), &CorsOrigins::Disabled);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // ディレクトリに置き換えると、存在しない場合とは違う読み取りエラーになる
    std::fs::remove_file(dir.join("live/720p/101.ts")).unwrap();
    std::fs::create_dir(dir.join("live/720p/101.ts")).unwrap();
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/segments/live/720p/101.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 500);
    let cache = state.cache.lock().await;
    assert!(!cache.contains("live", "720p", 101));
    let _ = std::fs::remove_dir_all(dir);
}

/// axum 7 の記法として解釈され得る文字を含むパスの接頭辞も、URL のリテラルとして配信する
#[tokio::test]
async fn serves_segments_under_path_prefix_with_route_characters() {
    for path in ["/foo/:tenant", "/foo/*", "/foo{bar}"] {
        let dir = temp_dir("route-characters");
        write_segment(&dir, "live", "720p", 101, b"segment-101");
        let cache = Cache::scan(dir.clone(), 12).await.unwrap();
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
        // 要求 URL では `{` と `}` が `%7B` `%7D` になる
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
        let _ = std::fs::remove_dir_all(dir);
    }
}
