//! controller と worker の HTTP 契約と統合動作を実サーバーで確認する

mod support;

use std::time::Duration;

use edamame_domain::CorsOrigins;
use serde_json::{Value, json};
use support::{
    TempDir, spawn_controller, spawn_controller_with_auth, spawn_controller_with_cors,
    spawn_controller_with_external_percent, spawn_controller_with_publish_wait, spawn_worker,
    spawn_worker_with_auth, spawn_worker_with_cors, spawn_worker_with_weight, wait_for_playlist,
    wait_for_playlist_contains, wait_for_segment,
};

fn any_addr() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

struct PublishInput<'a> {
    base_url: &'a str,
    quality: &'a str,
    identifier: &'a str,
    duration_ms: u64,
    bandwidth: u64,
    resolution: &'a str,
    body: &'a [u8],
}

async fn publish(client: &reqwest::Client, input: PublishInput<'_>) -> reqwest::Response {
    publish_with_token(client, input, None).await
}

/// 認証トークンを付けてセグメントを PUT する
async fn publish_with_token(
    client: &reqwest::Client,
    input: PublishInput<'_>,
    auth_token: Option<&str>,
) -> reqwest::Response {
    let request = client
        .put(format!(
            "{}/segments/live/{}/{}?duration_ms={}&bandwidth={}&resolution={}",
            input.base_url,
            input.quality,
            input.identifier,
            input.duration_ms,
            input.bandwidth,
            input.resolution
        ))
        .body(input.body.to_vec());
    let request = match auth_token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    request.send().await.unwrap()
}

/// 代表的な属性でセグメントを PUT し、割り当てられた番号を返す
async fn publish_segment(
    client: &reqwest::Client,
    base_url: &str,
    quality: &str,
    identifier: &str,
) -> u64 {
    publish_segment_with(client, base_url, quality, identifier, 3_000_000, "1280x720").await
}

/// quality ごとの属性を指定してセグメントを PUT し、割り当てられた番号を返す
async fn publish_segment_with(
    client: &reqwest::Client,
    base_url: &str,
    quality: &str,
    identifier: &str,
    bandwidth: u64,
    resolution: &str,
) -> u64 {
    let body = format!("segment-{quality}-{identifier}");
    let response = publish(
        client,
        PublishInput {
            base_url,
            quality,
            identifier,
            duration_ms: 6000,
            bandwidth,
            resolution,
            body: body.as_bytes(),
        },
    )
    .await;
    assert_eq!(
        response.status().as_u16(),
        201,
        "セグメントの PUT に失敗した"
    );
    let response_body: Value = response.json().await.unwrap();
    response_body["sequence"].as_u64().unwrap()
}

/// 連番の識別子でセグメントを PUT し、割り当てられた番号を返す
async fn publish_series(
    client: &reqwest::Client,
    base_url: &str,
    quality: &str,
    identifiers: impl IntoIterator<Item = u64>,
) -> Vec<u64> {
    let mut sequences = Vec::new();
    for index in identifiers {
        sequences.push(publish_segment(client, base_url, quality, &format!("seg-{index}")).await);
    }
    sequences
}

/// 鍵 URI を付けてセグメントを PUT する
async fn publish_with_key_uri(
    client: &reqwest::Client,
    base_url: &str,
    quality: &str,
    identifier: &str,
    key_uri: &str,
) -> reqwest::Response {
    let body = format!("segment-{quality}-{identifier}");
    // 鍵 URI は query や `&` を含み得るため、文字列連結ではなく query として組み立てる
    client
        .put(format!("{base_url}/segments/live/{quality}/{identifier}"))
        .query(&[
            ("duration_ms", "6000"),
            ("bandwidth", "3000000"),
            ("resolution", "1280x720"),
            ("key_uri", key_uri),
        ])
        .body(body)
        .send()
        .await
        .unwrap()
}

/// 鍵 URI を付けてセグメントを PUT し、割り当てられた番号を返す
async fn publish_segment_with_key(
    client: &reqwest::Client,
    base_url: &str,
    quality: &str,
    identifier: &str,
    key_uri: &str,
) -> u64 {
    let response = publish_with_key_uri(client, base_url, quality, identifier, key_uri).await;
    assert_eq!(
        response.status().as_u16(),
        201,
        "鍵付きセグメントの PUT に失敗した"
    );
    let body: Value = response.json().await.unwrap();
    body["sequence"].as_u64().unwrap()
}

fn cached(quality: &str, sequences: impl IntoIterator<Item = u64>) -> Vec<Value> {
    sequences
        .into_iter()
        .map(|sequence| {
            json!({
                "channel_id": "live",
                "quality": quality,
                "sequence": sequence
            })
        })
        .collect()
}

async fn register_worker(
    client: &reqwest::Client,
    base_url: &str,
    worker_id: &str,
    public_url: &str,
    weight: u32,
) -> reqwest::Response {
    client
        .put(format!("{base_url}/workers/{worker_id}"))
        .json(&json!({ "public_url": public_url, "weight": weight }))
        .send()
        .await
        .unwrap()
}

async fn sync_worker(
    client: &reqwest::Client,
    base_url: &str,
    worker_id: &str,
    cached_segments: Vec<Value>,
) -> reqwest::Response {
    client
        .post(format!("{base_url}/workers/{worker_id}/sync"))
        .json(&json!({ "cached_segments": cached_segments }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn segment_put_get_contract() {
    let tmp = TempDir::new("segment-contract");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;

    // 新しい識別子には 0 から連番を割り当てる
    assert_eq!(publish_segment(&client, base, "720p", "segment-1").await, 0);
    assert_eq!(publish_segment(&client, base, "720p", "segment-2").await, 1);

    // 同じ識別子で同じバイト列の再 PUT は、同じ番号を返してファイルを増やさない
    let repeated = publish(
        &client,
        PublishInput {
            base_url: base,
            quality: "720p",
            identifier: "segment-1",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: b"segment-720p-segment-1",
        },
    )
    .await;
    assert_eq!(repeated.status().as_u16(), 200);
    assert_eq!(
        repeated
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap(),
        "application/json"
    );
    let repeated_body: Value = repeated.json().await.unwrap();
    assert_eq!(repeated_body["sequence"], 0);

    // 同じ識別子で違うバイト列は 409 にする
    let conflict = publish(
        &client,
        PublishInput {
            base_url: base,
            quality: "720p",
            identifier: "segment-1",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: b"different",
        },
    )
    .await;
    assert_eq!(conflict.status().as_u16(), 409);

    let missing = client
        .put(format!("{base}/segments/live/720p/segment-3"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status().as_u16(), 400);
    let short = client
        .put(format!(
            "{base}/segments/live/720p/segment-3?duration_ms=400&bandwidth=3000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(short.status().as_u16(), 400);
    let get = client
        .get(format!("{base}/segments/live/720p/0.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status().as_u16(), 200);
    assert_eq!(
        get.bytes().await.unwrap().as_ref(),
        b"segment-720p-segment-1"
    );
    let not_found = client
        .get(format!("{base}/segments/live/720p/999.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(not_found.status().as_u16(), 404);
}

/// 同じセグメントファイルへの同時 PUT は、先に保存したバイト列を上書きしない
#[tokio::test]
async fn concurrent_put_keeps_first_bytes() {
    let tmp = TempDir::new("concurrent-put");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let url = format!(
        "{}/segments/live/720p/segment-a?duration_ms=6000&bandwidth=3000000&resolution=1280x720",
        controller.base_url
    );
    let first_body = vec![1_u8; 1024 * 1024];
    let second_body = vec![2_u8; 1024 * 1024];
    let (first, second) = tokio::join!(
        client.put(&url).body(first_body.clone()).send(),
        client.put(&url).body(second_body.clone()).send()
    );
    let first_status = first.unwrap().status().as_u16();
    let second_status = second.unwrap().status().as_u16();
    let mut statuses = [first_status, second_status];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409]);
    // 保存されたバイト列が、201 を返した側のバイト列であることを確認する
    let stored = tokio::fs::read(storage.join("live/720p/0.ts"))
        .await
        .unwrap();
    let expected = if first_status == 201 {
        &first_body
    } else {
        &second_body
    };
    assert_eq!(&stored, expected);
}

/// 属性が競合する同時 PUT は、メタデータを確立できなかった側のファイルを残さない
#[tokio::test]
async fn concurrent_attribute_conflict_leaves_no_file() {
    let tmp = TempDir::new("concurrent-attributes");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let first_url =
        format!("{base}/segments/live/720p/segment-a?duration_ms=6000&bandwidth=3000000");
    let second_url =
        format!("{base}/segments/live/720p/segment-b?duration_ms=7000&bandwidth=3000000");
    let (first, second) = tokio::join!(
        client.put(&first_url).body("segment-a").send(),
        client.put(&second_url).body("segment-b").send()
    );
    let first_status = first.unwrap().status().as_u16();
    let second_status = second.unwrap().status().as_u16();
    let mut statuses = [first_status, second_status];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409]);
    // 409 になった側のファイルを残すと、正しいバイト列のセグメントファイルを登録できなくなる
    assert!(storage.join("live/720p/0.ts").exists());
    assert!(!storage.join("live/720p/1.ts").exists());
}

/// 保存に失敗した PUT は、channel の属性とセグメント番号を確定しない
#[tokio::test]
async fn failed_write_does_not_establish_metadata() {
    let tmp = TempDir::new("failed-write");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    // quality のディレクトリの位置にファイルを置き、セグメントファイルの保存を失敗させる
    std::fs::create_dir_all(storage.join("live")).unwrap();
    std::fs::write(storage.join("live/720p"), b"block").unwrap();
    let failed = client
        .put(format!(
            "{base}/segments/live/720p/segment-1?duration_ms=6000&bandwidth=3000000"
        ))
        .body("segment-720p-segment-1")
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status().as_u16(), 500);

    // 失敗した PUT は属性も番号も確定しないため、違う duration_ms の再 PUT が 0 番として成功する
    std::fs::remove_file(storage.join("live/720p")).unwrap();
    let retried = client
        .put(format!(
            "{base}/segments/live/720p/segment-1?duration_ms=7000&bandwidth=3000000"
        ))
        .body("segment-720p-segment-1")
        .send()
        .await
        .unwrap();
    assert_eq!(retried.status().as_u16(), 201);
    let retried_body: Value = retried.json().await.unwrap();
    assert_eq!(retried_body["sequence"], 0);
}

#[tokio::test]
async fn segment_validation_and_conflicts() {
    let tmp = TempDir::new("segment-conflicts");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    assert_eq!(publish_segment(&client, base, "720p", "seg-0").await, 0);
    let duration_conflict = client
        .put(format!(
            "{base}/segments/live/720p/seg-1?duration_ms=7000&bandwidth=3000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(duration_conflict.status().as_u16(), 409);
    let bandwidth_conflict = client
        .put(format!(
            "{base}/segments/live/720p/seg-1?duration_ms=6000&bandwidth=4000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(bandwidth_conflict.status().as_u16(), 409);
    let resolution_conflict = client
        .put(format!(
            "{base}/segments/live/720p/seg-1?duration_ms=6000&bandwidth=3000000&resolution=1920x1080"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resolution_conflict.status().as_u16(), 409);
    let omitted_resolution = client
        .put(format!(
            "{base}/segments/live/720p/seg-1?duration_ms=6000&bandwidth=3000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(omitted_resolution.status().as_u16(), 201);
    let omitted_resolution_body: Value = omitted_resolution.json().await.unwrap();
    assert_eq!(omitted_resolution_body["sequence"], 1);
    // 別の quality でも同じ識別子には同じ番号を割り当てる
    let other_quality = client
        .put(format!(
            "{base}/segments/live/1080p/seg-1?duration_ms=6000&bandwidth=6000000&resolution=1920x1080"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(other_quality.status().as_u16(), 201);
    let other_quality_body: Value = other_quality.json().await.unwrap();
    assert_eq!(other_quality_body["sequence"], 1);
}

#[tokio::test]
async fn invalid_identifiers_are_rejected() {
    let tmp = TempDir::new("traversal");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage, 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let encoded_space = client
        .put(format!(
            "{base}/segments/bad%20channel/720p/segment-1?duration_ms=6000&bandwidth=3000000"
        ))
        .body("escape")
        .send()
        .await
        .unwrap();
    assert_eq!(encoded_space.status().as_u16(), 400);
    let encoded_backslash = client
        .put(format!(
            "{base}/segments/live%5C..%5Croot/720p/segment-1?duration_ms=6000&bandwidth=3000000"
        ))
        .body("escape")
        .send()
        .await
        .unwrap();
    assert_eq!(encoded_backslash.status().as_u16(), 400);
    assert!(!tmp.path.join("root").exists());
    // 識別子に使えるのは [A-Za-z0-9_-] だけとする
    let dot = client
        .put(format!(
            "{base}/segments/live/720p/segment.1?duration_ms=6000&bandwidth=3000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(dot.status().as_u16(), 400);
    let space = client
        .put(format!(
            "{base}/segments/live/720p/segment%201?duration_ms=6000&bandwidth=3000000"
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(space.status().as_u16(), 400);
    let too_long = client
        .put(format!(
            "{base}/segments/live/720p/{}?duration_ms=6000&bandwidth=3000000",
            "a".repeat(65)
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(too_long.status().as_u16(), 400);
    // 64 文字までは許可する
    let allowed = client
        .put(format!(
            "{base}/segments/live/720p/{}?duration_ms=6000&bandwidth=3000000",
            "a".repeat(64)
        ))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status().as_u16(), 201);
}

/// メディアプレイリストに書かれた worker を a と b の記号で返す
async fn selected_worker(client: &reqwest::Client, base_url: &str) -> &'static str {
    let response = client
        .get(format!("{base_url}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().await.unwrap();
    if body.contains("http://worker-a.test") {
        "a"
    } else {
        "b"
    }
}

/// 重みの比に従ってプレイリストの worker を選ぶ
#[tokio::test]
async fn playlist_selects_workers_by_weight() {
    let tmp = TempDir::new("weight");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker-a.test", 4).await;
    register_worker(&client, base, "worker-b", "http://worker-b.test", 1).await;
    for worker_id in ["worker-a", "worker-b"] {
        let sync = sync_worker(&client, base, worker_id, cached("720p", sequences.clone())).await;
        assert_eq!(sync.status().as_u16(), 200);
    }

    // 重み 4 と 1 では、5 回のうち 1 回だけ worker-b を選ぶ
    let mut selected = Vec::new();
    for _ in 0..5 {
        selected.push(selected_worker(&client, base).await);
    }
    assert_eq!(selected, vec!["a", "a", "b", "a", "a"]);
}

/// 重みを変えたら前の周期を引き継がず、新しい比で選び直す
#[tokio::test]
async fn weight_change_resets_selection_cycle() {
    let tmp = TempDir::new("weight-change");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker-a.test", 1000).await;
    register_worker(&client, base, "worker-b", "http://worker-b.test", 1).await;
    for worker_id in ["worker-a", "worker-b"] {
        let sync = sync_worker(&client, base, worker_id, cached("720p", sequences.clone())).await;
        assert_eq!(sync.status().as_u16(), 200);
    }
    // 大きい重みで選択の周期を作る
    for _ in 0..10 {
        assert_eq!(selected_worker(&client, base).await, "a");
    }

    // 重みを 1 に変えてキャッシュを報告し直すと、同じ重みとして順番に選ぶ
    register_worker(&client, base, "worker-a", "http://worker-a.test", 1).await;
    for worker_id in ["worker-a", "worker-b"] {
        let sync = sync_worker(&client, base, worker_id, cached("720p", sequences.clone())).await;
        assert_eq!(sync.status().as_u16(), 200);
    }
    let mut selected = Vec::new();
    for _ in 0..4 {
        selected.push(selected_worker(&client, base).await);
    }
    assert_eq!(selected, vec!["a", "b", "a", "b"]);
}

/// 重みが不正な登録は 400 にする
#[tokio::test]
async fn worker_registration_rejects_invalid_weight() {
    let tmp = TempDir::new("weight-invalid");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let zero = register_worker(&client, base, "worker-a", "http://worker.test", 0).await;
    assert_eq!(zero.status().as_u16(), 400);
    // 重みを省略した登録も 400 にする
    let missing = client
        .put(format!("{base}/workers/worker-a"))
        .json(&json!({ "public_url": "http://worker.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status().as_u16(), 400);
    let accepted = register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(accepted.status().as_u16(), 201);
}

/// worker は設定した重みを登録で報告する
#[tokio::test]
async fn worker_registers_configured_weight() {
    let tmp = TempDir::new("worker-weight");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let _worker = spawn_worker_with_weight(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
        4,
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let inner = controller.state.inner.lock().await;
            if let Some(worker) = inner.workers.get("worker-a") {
                assert_eq!(worker.weight, 4);
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker の登録が届かなかった"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn playlist_contract_with_ready_worker() {
    let tmp = TempDir::new("playlist");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    assert_eq!(sequences, vec![0, 1, 2, 3, 4, 5]);
    let unavailable = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status().as_u16(), 503);

    let worker_public_url = "http://worker.test";
    let registration = register_worker(&client, base, "worker-a", worker_public_url, 1).await;
    assert_eq!(registration.status().as_u16(), 201);
    let registration_body: Value = registration.json().await.unwrap();
    assert_eq!(registration_body["sync_interval_ms"], 1000);

    let sync = sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;
    assert_eq!(sync.status().as_u16(), 200);
    let sync_body: Value = sync.json().await.unwrap();
    assert!(sync_body["fetch_segments"].as_array().unwrap().is_empty());

    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    assert_eq!(
        media.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        "application/vnd.apple.mpegurl"
    );
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-TARGETDURATION:6"));
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(media_body.contains("#EXTINF:6.000,"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/5.ts"));

    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720"));
    assert!(master_body.contains("720p.m3u8"));
}

#[tokio::test]
async fn playlist_gap_from_missing_quality_segment_is_filled() {
    let tmp = TempDir::new("gap");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..3).await;
    // 480p は seg-1 を欠いたまま PUT する
    assert_eq!(publish_segment(&client, base, "480p", "seg-0").await, 0);
    assert_eq!(publish_segment(&client, base, "480p", "seg-2").await, 2);
    let unknown = client
        .get(format!("{base}/channels/unknown/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 404);
    let gap = client
        .get(format!("{base}/channels/live/480p.m3u8"))
        .send()
        .await
        .unwrap();
    // 欠落した範囲はまだ公開しない
    assert_eq!(gap.status().as_u16(), 503);

    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    let sync = sync_worker(&client, base, "worker-a", cached("720p", 0..3)).await;
    assert_eq!(sync.status().as_u16(), 200);
    let gap_with_worker = client
        .get(format!("{base}/channels/live/480p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(gap_with_worker.status().as_u16(), 503);

    // 欠けた識別子は、別の quality が先に割り当てた番号を使う
    assert_eq!(publish_segment(&client, base, "480p", "seg-1").await, 1);
    let mut cached_segments = cached("720p", 0..3);
    cached_segments.extend(cached("480p", 0..3));
    let sync = sync_worker(&client, base, "worker-a", cached_segments).await;
    assert_eq!(sync.status().as_u16(), 200);
    let filled = client
        .get(format!("{base}/channels/live/480p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(filled.status().as_u16(), 200);
    let filled_body = filled.text().await.unwrap();
    assert!(filled_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(filled_body.contains("http://worker.test/segments/live/480p/2.ts"));
}

/// 同じ識別子を複数 quality に PUT すると同じ番号になり、マスタープレイリストを生成できる
#[tokio::test]
async fn master_playlist_aligns_qualities_by_identifier() {
    let tmp = TempDir::new("master-align");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    // quality ごとに PUT する順序を入れ替えても、同じ識別子には同じ番号を割り当てる
    let mut sequences_720p = Vec::new();
    let mut sequences_480p = Vec::new();
    for index in 0..6 {
        let identifier = format!("seg-{index}");
        if index % 2 == 0 {
            sequences_720p.push(
                publish_segment_with(&client, base, "720p", &identifier, 3_000_000, "1280x720")
                    .await,
            );
            sequences_480p.push(
                publish_segment_with(&client, base, "480p", &identifier, 1_000_000, "854x480")
                    .await,
            );
        } else {
            sequences_480p.push(
                publish_segment_with(&client, base, "480p", &identifier, 1_000_000, "854x480")
                    .await,
            );
            sequences_720p.push(
                publish_segment_with(&client, base, "720p", &identifier, 3_000_000, "1280x720")
                    .await,
            );
        }
    }
    assert_eq!(sequences_720p, vec![0, 1, 2, 3, 4, 5]);
    assert_eq!(sequences_720p, sequences_480p);

    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    let mut cached_segments = cached("720p", sequences_720p);
    cached_segments.extend(cached("480p", sequences_480p));
    sync_worker(&client, base, "worker-a", cached_segments).await;

    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("480p.m3u8"));
    assert!(master_body.contains("720p.m3u8"));
    for quality in ["480p", "720p"] {
        let media = client
            .get(format!("{base}/channels/live/{quality}.m3u8"))
            .send()
            .await
            .unwrap();
        assert_eq!(media.status().as_u16(), 200);
        let media_body = media.text().await.unwrap();
        assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(media_body.contains(&format!("http://worker.test/segments/live/{quality}/5.ts")));
    }
}

/// 公開予定時刻を過ぎたら、sync で報告された新しいセグメントをすぐ反映する
#[tokio::test]
async fn media_playlist_reflects_new_segment() {
    let tmp = TempDir::new("no-stale");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    let first = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);
    let first_body = first.text().await.unwrap();
    assert!(first_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    let mut all = sequences;
    all.push(publish_segment(&client, base, "720p", "seg-6").await);
    sync_worker(&client, base, "worker-a", cached("720p", all)).await;
    let second = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    let second_body = second.text().await.unwrap();
    assert!(second_body.contains("#EXT-X-MEDIA-SEQUENCE:1"));
    assert!(second_body.contains("http://worker.test/segments/live/720p/6.ts"));
    assert!(!second_body.contains("http://worker.test/segments/live/720p/0.ts"));
}

/// 鍵 URI はセグメント PUT からメディアプレイリストまで通り、省略した再 PUT で外れる
#[tokio::test]
async fn key_uri_is_reflected_in_media_playlist() {
    let tmp = TempDir::new("key-uri");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let key = "https://keys.test/live/a";
    let mut sequences = Vec::new();
    for index in 0..6 {
        sequences.push(
            publish_segment_with_key(&client, base, "720p", &format!("seg-{index}"), key).await,
        );
    }
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;

    let playlist = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(playlist.status().as_u16(), 200);
    let body = playlist.text().await.unwrap();
    assert!(body.contains(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{key}\"\n")));

    // 鍵 URI は競合する属性ではないため、省略した再 PUT は 200 で通り、暗号化なしの区間が出る
    let reput = client
        .put(format!(
            "{base}/segments/live/720p/seg-3?duration_ms=6000&bandwidth=3000000&resolution=1280x720"
        ))
        .body("segment-720p-seg-3")
        .send()
        .await
        .unwrap();
    assert_eq!(reput.status().as_u16(), 200);

    let playlist = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    let body = playlist.text().await.unwrap();
    assert!(body.contains("#EXT-X-KEY:METHOD=NONE\n"));
}

/// 鍵 URI は絶対 http URL だけを受け付ける
#[tokio::test]
async fn key_uri_validation_rejects_invalid_urls() {
    let tmp = TempDir::new("key-uri-invalid");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    for (index, key_uri) in [
        "/keys/key-1",
        "ftp://keys.test/key",
        "https://keys.test/key#fragment",
    ]
    .iter()
    .enumerate()
    {
        let response =
            publish_with_key_uri(&client, base, "720p", &format!("bad-{index}"), key_uri).await;
        assert_eq!(
            response.status().as_u16(),
            400,
            "不正な key_uri を受け付けた: {key_uri}"
        );
    }
    // 鍵の配信元が認証に使うことがあるため、query 付きの https は許可する
    let accepted = publish_with_key_uri(
        &client,
        base,
        "720p",
        "good",
        "https://keys.test/key?token=1",
    )
    .await;
    assert_eq!(accepted.status().as_u16(), 201);
}

/// 公開待ちの間は前の範囲を返し、待ち時間が過ぎたら追いついた worker に合わせて公開範囲を進める
#[tokio::test]
async fn publish_wait_keeps_previous_playlist_until_worker_catches_up() {
    let tmp = TempDir::new("publish-wait");
    let controller = spawn_controller_with_publish_wait(
        any_addr(),
        tmp.path.join("storage"),
        6,
        1000,
        10000,
        300,
    )
    .await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;

    // worker がまだない間は公開範囲がないため 503 になる
    let unavailable = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status().as_u16(), 503);

    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    let initial_body = wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;
    assert!(initial_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    // 未公開セグメントを PUT しても、待ち時間が過ぎるまでは前の範囲を返す
    let mut all = sequences;
    all.push(publish_segment(&client, base, "720p", "seg-6").await);
    let waiting = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(waiting.status().as_u16(), 200);
    let waiting_body = waiting.text().await.unwrap();
    assert!(waiting_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(!waiting_body.contains("/6.ts"));

    // 待ち時間内に worker が取得しても、次の公開判定までは前の範囲を返す
    let sync = sync_worker(&client, base, "worker-a", cached("720p", all.clone())).await;
    assert_eq!(sync.status().as_u16(), 200);
    let still_waiting = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(still_waiting.status().as_u16(), 200);
    let still_waiting_body = still_waiting.text().await.unwrap();
    assert!(still_waiting_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    // 待ち時間が過ぎても、次の sync までは公開範囲を変えない
    tokio::time::sleep(Duration::from_millis(350)).await;
    let still_waiting_after_due = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(still_waiting_after_due.status().as_u16(), 200);
    let still_waiting_after_due_body = still_waiting_after_due.text().await.unwrap();
    assert!(still_waiting_after_due_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    // 待ち時間後の sync で、キャッシュ済みの範囲に合わせて公開範囲を進める
    sync_worker(&client, base, "worker-a", cached("720p", all)).await;
    let published = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(published.status().as_u16(), 200);
    let published_body = published.text().await.unwrap();
    assert!(published_body.contains("#EXT-X-MEDIA-SEQUENCE:1"));
    assert!(published_body.contains("http://worker.test/segments/live/720p/6.ts"));
    assert!(!published_body.contains("http://worker.test/segments/live/720p/0.ts"));
}

/// 期限切れの公開待ちの後に PUT されたセグメントは、新しい公開待ちになる
#[tokio::test]
async fn starts_new_publish_wait_after_expired_batch() {
    let tmp = TempDir::new("expired-wait");
    let controller = spawn_controller_with_publish_wait(
        any_addr(),
        tmp.path.join("storage"),
        6,
        1000,
        10000,
        300,
    )
    .await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    // 最初の公開待ちが切れた後に次のセグメントを PUT する
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    let mut all = sequences;
    all.push(6);
    sync_worker(&client, base, "worker-a", cached("720p", all.clone())).await;

    // 6 には新しい公開待ちがあるため、まだ公開しない
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(!media_body.contains("http://worker.test/segments/live/720p/6.ts"));

    tokio::time::sleep(Duration::from_millis(350)).await;
    sync_worker(&client, base, "worker-a", cached("720p", all)).await;
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:1"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/6.ts"));
}

/// prune 済みのセグメントを同じバイト列で再 PUT しても、新しい公開待ちは始めない
#[tokio::test]
async fn reput_pruned_segment_does_not_start_wait() {
    let tmp = TempDir::new("reput-pruned");
    let controller = spawn_controller_with_publish_wait(
        any_addr(),
        tmp.path.join("storage"),
        6,
        1000,
        10000,
        2000,
    )
    .await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    tokio::time::sleep(Duration::from_millis(2050)).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;

    // seg-6 でメタデータを 1..6 に prune する
    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    sync_worker(&client, base, "worker-a", cached("720p", 0..7)).await;

    let repeated = publish(
        &client,
        PublishInput {
            base_url: base,
            quality: "720p",
            identifier: "seg-0",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: b"segment-720p-seg-0",
        },
    )
    .await;
    assert_eq!(repeated.status().as_u16(), 200);

    // worker が 0 だけを報告しても、再 PUT した 0 は公開待ちに入らず 503 にならない
    sync_worker(&client, base, "worker-a", cached("720p", 0..1)).await;
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/0.ts"));
}

/// 新しい quality は worker が公開範囲を取り込むまで master playlist に追加しない
#[tokio::test]
async fn master_playlist_excludes_quality_until_published() {
    let tmp = TempDir::new("master-wait");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;

    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("720p.m3u8"));
    assert!(!master_body.contains("480p.m3u8"));

    // 480p を PUT しても worker が取り込むまで 480p は公開しない
    publish_series(&client, base, "480p", 0..6).await;
    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("720p.m3u8"));
    assert!(!master_body.contains("480p.m3u8"));
    assert_eq!(
        client
            .get(format!("{base}/channels/live/480p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );

    let mut cached_segments = cached("720p", sequences);
    cached_segments.extend(cached("480p", 0..6));
    sync_worker(&client, base, "worker-a", cached_segments).await;
    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("480p.m3u8"));
    assert!(master_body.contains("720p.m3u8"));
    assert_eq!(
        client
            .get(format!("{base}/channels/live/480p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
}

/// 公開範囲を持たない worker は使わず、後から追いついた worker は配信先に復帰する
#[tokio::test]
async fn uses_only_worker_that_has_published_range() {
    let tmp = TempDir::new("worker-published");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker-a.test", 1).await;
    register_worker(&client, base, "worker-b", "http://worker-b.test", 1).await;
    sync_worker(&client, base, "worker-b", cached("720p", sequences.clone())).await;

    // worker-a はまだ公開範囲を持たないため、追いついた worker-b を使う
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("http://worker-b.test/segments/live/720p/5.ts"));

    // worker-a が追いつけば worker_id 昇順で worker-a に戻る
    sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("http://worker-a.test/segments/live/720p/5.ts"));
}

/// quality ごとに別の worker が公開範囲を配信できる
#[tokio::test]
async fn keeps_serving_when_qualities_advance_at_different_times() {
    let tmp = TempDir::new("quality-workers");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let initial_720p = publish_series(&client, base, "720p", 0..6).await;
    let initial_480p = publish_series(&client, base, "480p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker-a.test", 1).await;
    register_worker(&client, base, "worker-b", "http://worker-b.test", 1).await;

    // 最初は両方の quality を worker-a から配信する
    let mut cached_a = cached("720p", initial_720p);
    cached_a.extend(cached("480p", initial_480p));
    sync_worker(&client, base, "worker-a", cached_a).await;

    // worker-a は 720p だけ新しく、worker-b は 480p だけ新しい
    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    assert_eq!(publish_segment(&client, base, "480p", "seg-6").await, 6);
    let mut cached_a = cached("720p", 0..7);
    cached_a.extend(cached("480p", 0..6));
    sync_worker(&client, base, "worker-a", cached_a).await;
    let mut cached_b = cached("720p", 0..6);
    cached_b.extend(cached("480p", 0..7));
    sync_worker(&client, base, "worker-b", cached_b).await;

    // 720p は worker-a、480p は worker-b が配信する
    let media_720p = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media_720p.status().as_u16(), 200);
    let media_720p_body = media_720p.text().await.unwrap();
    assert!(media_720p_body.contains("http://worker-a.test/"));
    assert!(media_720p_body.contains("/6.ts"));

    let media_480p = client
        .get(format!("{base}/channels/live/480p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media_480p.status().as_u16(), 200);
    let media_480p_body = media_480p.text().await.unwrap();
    assert!(media_480p_body.contains("http://worker-b.test/"));
    assert!(media_480p_body.contains("/6.ts"));
}

/// 古い公開範囲を追い出したら、sync で既公開範囲へフォールバックする
#[tokio::test]
async fn publishes_when_worker_evicted_previous_range() {
    let tmp = TempDir::new("evicted-published");
    let controller = spawn_controller_with_publish_wait(
        any_addr(),
        tmp.path.join("storage"),
        6,
        1000,
        10000,
        1000,
    )
    .await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;

    // 公開待ちの間に 3 個進み、worker は古い公開範囲の先頭を追い出して最新もまだ未取得である
    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    assert_eq!(publish_segment(&client, base, "720p", "seg-7").await, 7);
    assert_eq!(publish_segment(&client, base, "720p", "seg-8").await, 8);
    sync_worker(&client, base, "worker-a", cached("720p", 1..8)).await;

    // 未公開の 6..8 はまだ使わず、sync で既公開の 1..5 へフォールバックする
    let waiting = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(waiting.status().as_u16(), 200);
    let waiting_body = waiting.text().await.unwrap();
    assert!(waiting_body.contains("#EXT-X-MEDIA-SEQUENCE:1"));
    assert!(waiting_body.contains("http://worker.test/segments/live/720p/5.ts"));
    assert!(!waiting_body.contains("http://worker.test/segments/live/720p/6.ts"));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    sync_worker(&client, base, "worker-a", cached("720p", 1..8)).await;
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        media.status().as_u16(),
        200,
        "古い範囲の追い出しで 503 になった"
    );
    let media_body = media.text().await.unwrap();
    // 待ち時間が過ぎたら、保持している最新範囲 2..7 へ進める
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:2"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/7.ts"));
    assert!(!media_body.contains("http://worker.test/segments/live/720p/8.ts"));

    // 最新まで取得できれば、次の sync で公開範囲が最新に追いつく
    sync_worker(&client, base, "worker-a", cached("720p", 0..9)).await;
    let media_body = wait_for_playlist_contains(
        &client,
        &format!("{base}/channels/live/720p.m3u8"),
        "http://worker.test/segments/live/720p/8.ts",
    )
    .await;
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:3"));
}

/// 最新側に欠落があっても、待ち時間後の sync で最新の連続範囲へ進める
#[tokio::test]
async fn publishes_connectable_range_when_latest_has_a_gap() {
    let tmp = TempDir::new("connectable-gap");
    let controller = spawn_controller_with_publish_wait(
        any_addr(),
        tmp.path.join("storage"),
        6,
        1000,
        10000,
        1000,
    )
    .await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;

    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    assert_eq!(publish_segment(&client, base, "720p", "seg-7").await, 7);
    assert_eq!(publish_segment(&client, base, "720p", "seg-8").await, 8);
    // 3..6 は保持しているが 7 は欠落し、8 だけを保持している
    let mut cached_segments = cached("720p", 3..7);
    cached_segments.extend(cached("720p", 8..9));
    sync_worker(&client, base, "worker-a", cached_segments.clone()).await;

    // 未公開の 6..8 はまだ使わず、sync で既公開の 3..5 へフォールバックする
    let waiting = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(waiting.status().as_u16(), 200);
    let waiting_body = waiting.text().await.unwrap();
    assert!(waiting_body.contains("#EXT-X-MEDIA-SEQUENCE:3"));
    assert!(waiting_body.contains("http://worker.test/segments/live/720p/5.ts"));
    assert!(!waiting_body.contains("http://worker.test/segments/live/720p/6.ts"));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    sync_worker(&client, base, "worker-a", cached_segments.clone()).await;
    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        media.status().as_u16(),
        200,
        "接続可能な範囲があるのに 503 になった"
    );
    let media_body = media.text().await.unwrap();
    // 最新の連続範囲は 8 だけなので、そこへ進める
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:8"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/8.ts"));
    assert!(!media_body.contains("http://worker.test/segments/live/720p/6.ts"));
}

/// 公開先頭より古い範囲しか配信できない場合、media sequence は後退する
#[tokio::test]
async fn allows_media_sequence_to_move_backwards() {
    let tmp = TempDir::new("media-sequence");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    // 最初はセグメント 5 だけを公開する
    sync_worker(&client, base, "worker-a", cached("720p", 5..6)).await;
    let media_body = wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:5"));

    // メタデータを 3..8 に進め、worker は 3..8 を保持している
    assert_eq!(publish_segment(&client, base, "720p", "seg-6").await, 6);
    assert_eq!(publish_segment(&client, base, "720p", "seg-7").await, 7);
    assert_eq!(publish_segment(&client, base, "720p", "seg-8").await, 8);
    sync_worker(&client, base, "worker-a", cached("720p", 3..9)).await;

    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200);
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:3"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/3.ts"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/8.ts"));
}

/// 直近範囲より前を取り逃がした worker でも、保持している最新範囲へ復旧できる
#[tokio::test]
async fn recovers_when_worker_is_farther_behind_than_the_window() {
    let tmp = TempDir::new("recovery");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    wait_for_playlist(&client, &format!("{base}/channels/live/720p.m3u8")).await;

    // 直近範囲の 6 個を超えて進み、古い範囲はメタデータから消える
    for index in 6..21 {
        assert_eq!(
            publish_segment(&client, base, "720p", &format!("seg-{index}")).await,
            index
        );
    }
    sync_worker(&client, base, "worker-a", cached("720p", 15..21)).await;

    let media = client
        .get(format!("{base}/channels/live/720p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(media.status().as_u16(), 200, "復旧できず 503 になった");
    let media_body = media.text().await.unwrap();
    assert!(media_body.contains("#EXT-X-MEDIA-SEQUENCE:15"));
    assert!(media_body.contains("http://worker.test/segments/live/720p/20.ts"));
    assert!(!media_body.contains("http://worker.test/segments/live/720p/0.ts"));
}

/// quality ごとに異なる範囲を、それぞれ別の worker から配信できる
#[tokio::test]
async fn qualities_publish_different_ranges_with_different_workers() {
    let tmp = TempDir::new("per-quality-ranges");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    // 720p は 0..8、480p は同じ識別子で 0..5 まで PUT する
    publish_series(&client, base, "720p", 0..9).await;
    publish_series(&client, base, "480p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker-a.test", 1).await;
    register_worker(&client, base, "worker-b", "http://worker-b.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..9)).await;
    sync_worker(&client, base, "worker-b", cached("480p", 0..6)).await;

    let master = client
        .get(format!("{base}/channels/live/master.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(master.status().as_u16(), 200);
    let master_body = master.text().await.unwrap();
    assert!(master_body.contains("480p.m3u8"));
    assert!(master_body.contains("720p.m3u8"));

    for (quality, sequence, worker_url) in [
        ("720p", 3, "http://worker-a.test/"),
        ("480p", 0, "http://worker-b.test/"),
    ] {
        let media = client
            .get(format!("{base}/channels/live/{quality}.m3u8"))
            .send()
            .await
            .unwrap();
        assert_eq!(media.status().as_u16(), 200, "{quality} が 503 になった");
        let media_body = media.text().await.unwrap();
        assert!(media_body.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{sequence}")));
        assert!(media_body.contains(worker_url));
    }
}

#[tokio::test]
async fn file_deleted_in_range_returns_422() {
    let tmp = TempDir::new("deleted");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    std::fs::remove_file(storage.join("live/720p/2.ts")).unwrap();
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        422
    );
}

/// 配信先 worker がいなくても、公開範囲のファイル欠落は 422 を優先する
#[tokio::test]
async fn file_deleted_without_worker_returns_422() {
    let tmp = TempDir::new("deleted-no-worker");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;

    std::fs::remove_file(storage.join("live/720p/2.ts")).unwrap();
    // 再登録でキャッシュを無効にし、公開範囲を保持する worker をなくす
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        422
    );
}

#[tokio::test]
async fn worker_registration_and_sync_contract() {
    let tmp = TempDir::new("worker-contract");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    let created = register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(created.status().as_u16(), 201);
    let updated = register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(updated.status().as_u16(), 200);
    // 最新の 5 がまだなくても、連続して保持している 0..4 を公開する
    let partial = sync_worker(&client, base, "worker-a", cached("720p", 0..5)).await;
    let partial_body: Value = partial.json().await.unwrap();
    assert_eq!(
        partial_body["fetch_segments"],
        json!([{ "channel_id": "live", "quality": "720p", "sequence": 5 }])
    );
    let ready = sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    let ready_body: Value = ready.json().await.unwrap();
    assert!(ready_body["fetch_segments"].as_array().unwrap().is_empty());
    let unknown = sync_worker(&client, base, "unknown", cached("720p", 0..6)).await;
    assert_eq!(unknown.status().as_u16(), 404);
}

/// JSON の形式が不正な入力は、ハンドラに届く前に 400 にする
#[tokio::test]
async fn invalid_json_returns_400() {
    let tmp = TempDir::new("invalid-json");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    // public_url の型が違う登録は 400 にする
    let bad_type = client
        .put(format!("{}/workers/worker-a", controller.base_url))
        .json(&json!({ "public_url": 1, "weight": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_type.status().as_u16(), 400);
    assert_eq!(
        bad_type
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap(),
        "text/plain; charset=utf-8"
    );
    // public_url が欠けた登録は 400 にする
    let missing_field = client
        .put(format!("{}/workers/worker-a", controller.base_url))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_field.status().as_u16(), 400);
    register_worker(
        &client,
        &controller.base_url,
        "worker-a",
        "http://worker.test",
        1,
    )
    .await;
    // cached_segments の型が違う sync は 400 にする
    let bad_sync = client
        .post(format!("{}/workers/worker-a/sync", controller.base_url))
        .json(&json!({ "cached_segments": "none" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_sync.status().as_u16(), 400);
    // cached_segments が null の sync は 400 にする
    let null_sync = client
        .post(format!("{}/workers/worker-a/sync", controller.base_url))
        .json(&json!({ "cached_segments": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(null_sync.status().as_u16(), 400);
    // 壊れた JSON も 400 にする
    let broken_sync = client
        .post(format!("{}/workers/worker-a/sync", controller.base_url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(broken_sync.status().as_u16(), 400);
}

/// JSON ボディの上限を超える登録は 400 にする
#[tokio::test]
async fn oversized_json_body_returns_400() {
    let tmp = TempDir::new("json-limit");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    // 4 MiB を超える public_url を作る
    let body = format!(
        r#"{{"public_url":"http://worker.test/{}"}}"#,
        "a".repeat(5 * 1024 * 1024)
    );
    assert!(body.len() > 4 * 1024 * 1024);
    let rejected = client
        .put(format!("{}/workers/worker-a", controller.base_url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status().as_u16(), 400);
}

#[tokio::test]
async fn worker_timeout_and_recovery() {
    let tmp = TempDir::new("worker-timeout");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 100).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn registration_update_does_not_extend_timeout() {
    let tmp = TempDir::new("registration-timeout");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 100).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let updated = register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(updated.status().as_u16(), 200);
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );
}

#[tokio::test]
async fn large_segment_body_is_accepted() {
    let tmp = TempDir::new("large-body");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let body = vec![7_u8; 3 * 1024 * 1024];
    let response = publish(
        &client,
        PublishInput {
            base_url: &controller.base_url,
            quality: "720p",
            identifier: "segment-1",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: &body,
        },
    )
    .await;
    assert_eq!(response.status().as_u16(), 201);
}

/// 再起動すると番号が 0 に戻り、同じ番号のファイルは新しいセグメントで上書きされる
#[tokio::test]
async fn controller_restart_restarts_numbering() {
    let tmp = TempDir::new("restart-numbering");
    let storage = tmp.path.join("storage");
    let first = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let first_addr = first.addr;
    let client = reqwest::Client::new();
    assert_eq!(
        publish_segment(&client, &first.base_url, "720p", "segment-1").await,
        0
    );
    assert_eq!(
        publish_segment_with(
            &client,
            &first.base_url,
            "480p",
            "segment-1",
            1_000_000,
            "854x480"
        )
        .await,
        0
    );
    assert_eq!(
        publish_segment(&client, &first.base_url, "720p", "segment-2").await,
        1
    );
    first.shutdown().await;

    let second = spawn_controller(first_addr, storage.clone(), 6, 1000, 10000).await;
    // 新しい実行では 0 から採番し、前の起動のファイルを上書きする
    let body = "restarted-segment";
    let response = publish(
        &client,
        PublishInput {
            base_url: &second.base_url,
            quality: "720p",
            identifier: "segment-1",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: body.as_bytes(),
        },
    )
    .await;
    assert_eq!(response.status().as_u16(), 201);
    let response_body: Value = response.json().await.unwrap();
    assert_eq!(response_body["sequence"], 0);
    // 別の quality への同じ識別子の初回 PUT も、前の実行のファイルを上書きする
    let other_quality = publish(
        &client,
        PublishInput {
            base_url: &second.base_url,
            quality: "480p",
            identifier: "segment-1",
            duration_ms: 6000,
            bandwidth: 1_000_000,
            resolution: "854x480",
            body: b"restarted-480p",
        },
    )
    .await;
    assert_eq!(other_quality.status().as_u16(), 201);
    let other_quality_body: Value = other_quality.json().await.unwrap();
    assert_eq!(other_quality_body["sequence"], 0);
    let stored = tokio::fs::read(storage.join("live/720p/0.ts"))
        .await
        .unwrap();
    assert_eq!(stored, body.as_bytes());
    // 新しい実行がまだ到達していない番号のファイルは残る
    assert!(storage.join("live/720p/1.ts").exists());
}

/// 登録を更新すると報告済みのキャッシュを無効にし、配信先に使わなくなる
#[tokio::test]
async fn registration_resets_cached_segments() {
    let tmp = TempDir::new("registration-reset");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    sync_worker(&client, base, "worker-a", cached("720p", sequences.clone())).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );

    // worker は登録のたびにキャッシュを捨てるため、登録を更新した時点で配信先に使えなくなる
    let updated = register_worker(&client, base, "worker-a", "http://worker.test", 1).await;
    assert_eq!(updated.status().as_u16(), 200);
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );

    // 次の sync で報告し直せば配信先に戻る
    sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;
    assert_eq!(
        client
            .get(format!("{base}/channels/live/720p.m3u8"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn end_to_end_publish_sync_playlist_serve() {
    let tmp = TempDir::new("e2e");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 50, 1000).await;
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_series(&client, &controller.base_url, "720p", 0..6).await;
    let media = wait_for_playlist_contains(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
        &format!("{}/segments/live/720p/5.ts", worker.base_url),
    )
    .await;
    assert!(media.contains(&format!("{}/segments/live/720p/5.ts", worker.base_url)));
    let segment = client
        .get(format!("{}/segments/live/720p/5.ts", worker.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(segment.status().as_u16(), 200);
    assert_eq!(
        segment.bytes().await.unwrap().as_ref(),
        b"segment-720p-seg-5"
    );
}

/// 連続してセグメントを PUT しても、公開待ちの間に playlist が 503 にならない
#[tokio::test]
async fn continuous_publish_keeps_playlist_available() {
    let tmp = TempDir::new("continuous-publish");
    let controller =
        spawn_controller_with_publish_wait(any_addr(), tmp.path.join("storage"), 6, 50, 1000, 200)
            .await;
    // 前の公開範囲と新しい範囲を同時に保持できるよう、playlist_segments より 1 つ多くする
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        7,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    let url = format!("{}/channels/live/720p.m3u8", controller.base_url);
    publish_series(&client, &controller.base_url, "720p", 0..6).await;
    wait_for_playlist(&client, &url).await;

    for index in 6..9 {
        assert_eq!(
            publish_segment(
                &client,
                &controller.base_url,
                "720p",
                &format!("seg-{index}")
            )
            .await,
            index
        );
        // 公開範囲が進むまでは前の範囲を返し、503 にはしない
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "公開待ちの間に 503 になった"
        );
        wait_for_playlist_contains(&client, &url, &format!("/{index}.ts")).await;
    }

    wait_for_segment(
        &client,
        &format!("{}/segments/live/720p/8.ts", worker.base_url),
        b"segment-720p-seg-8",
    )
    .await;
}

#[tokio::test]
async fn controller_restart_recovery() {
    let tmp = TempDir::new("controller-restart");
    let storage = tmp.path.join("storage");
    let first = spawn_controller(any_addr(), storage.clone(), 6, 50, 1000).await;
    let first_addr = first.addr;
    let first_base_url = first.base_url.clone();
    let worker = spawn_worker(
        "worker-a",
        &first_base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_series(&client, &first_base_url, "720p", 0..6).await;
    let first_media = wait_for_playlist(
        &client,
        &format!("{}/channels/live/720p.m3u8", first_base_url),
    )
    .await;
    assert!(first_media.contains(&worker.base_url));
    wait_for_segment(
        &client,
        &format!("{}/segments/live/720p/5.ts", worker.base_url),
        b"segment-720p-seg-5",
    )
    .await;

    first.shutdown().await;
    let second = spawn_controller(first_addr, storage, 6, 50, 1000).await;
    // 再起動後は同じ番号に別のセグメントを PUT する
    let restarted = publish_series(&client, &second.base_url, "720p", 10..16).await;
    assert_eq!(restarted, vec![0, 1, 2, 3, 4, 5]);
    let second_media = wait_for_playlist(
        &client,
        &format!("{}/channels/live/720p.m3u8", second.base_url),
    )
    .await;
    assert!(second_media.contains(&worker.base_url));
    assert!(second_media.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    // worker は登録時にキャッシュを捨てるため、古い内容ではなく新しい内容を配信する
    wait_for_segment(
        &client,
        &format!("{}/segments/live/720p/5.ts", worker.base_url),
        b"segment-720p-seg-15",
    )
    .await;
}

/// ブラウザからは許可した origin の GET だけを CORS で許可する
#[tokio::test]
async fn cors_headers_follow_configured_origins() {
    let tmp = TempDir::new("cors");
    let allowed_origin = "http://page.test";
    let controller = spawn_controller_with_cors(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        CorsOrigins::parse(&[allowed_origin.to_owned()]).unwrap(),
    )
    .await;
    let worker = spawn_worker_with_cors(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
        CorsOrigins::Any,
    )
    .await;
    let client = reqwest::Client::new();
    let sequences = publish_series(&client, &controller.base_url, "720p", 0..6).await;
    assert_eq!(sequences, vec![0, 1, 2, 3, 4, 5]);
    wait_for_playlist(
        &client,
        &format!("{}/channels/live/master.m3u8", controller.base_url),
    )
    .await;

    // 許可した origin からの読み取りには、その origin をそのまま返す
    let allowed = client
        .get(format!("{}/channels/live/master.m3u8", controller.base_url))
        .header(reqwest::header::ORIGIN, allowed_origin)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status().as_u16(), 200);
    assert_eq!(
        allowed
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        allowed_origin
    );

    // 許可していない origin には CORS ヘッダを付けない
    let denied = client
        .get(format!("{}/channels/live/master.m3u8", controller.base_url))
        .header(reqwest::header::ORIGIN, "http://other.test")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 200);
    assert!(
        denied
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "許可していない origin に CORS ヘッダが付いている"
    );

    // worker は `*` を指定しているため、すべての origin に `*` を返す
    let segment = client
        .get(format!("{}/segments/live/720p/0.ts", worker.base_url))
        .header(reqwest::header::ORIGIN, allowed_origin)
        .send()
        .await
        .unwrap();
    assert_eq!(segment.status().as_u16(), 200);
    assert_eq!(
        segment
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );

    // 書き込み系は cross-origin から実行させないため、PUT には CORS ヘッダを付けない
    let put = client
        .put(format!(
            "{}/segments/live/720p/seg-0?duration_ms=6000&bandwidth=3000000&resolution=1280x720",
            controller.base_url
        ))
        .header(reqwest::header::ORIGIN, allowed_origin)
        .body("segment-720p-seg-0")
        .send()
        .await
        .unwrap();
    assert_eq!(put.status().as_u16(), 200);
    assert!(
        put.headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "書き込み系に CORS ヘッダが付いている"
    );
}

/// worker は登録時にキャッシュを捨てて、controller のセグメントファイルを取り直す
#[tokio::test]
async fn worker_discards_cache_on_registration() {
    let tmp = TempDir::new("worker-cache");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 50, 1000).await;
    let cache_dir = tmp.path.join("cache");
    tokio::fs::create_dir_all(cache_dir.join("live/720p"))
        .await
        .unwrap();
    // 前の実行のセグメントファイルをキャッシュに置いておく
    for sequence in 0..6 {
        tokio::fs::write(
            cache_dir.join(format!("live/720p/{sequence}.ts")),
            format!("old-segment-{sequence}"),
        )
        .await
        .unwrap();
    }
    let client = reqwest::Client::new();
    publish_series(&client, &controller.base_url, "720p", 0..6).await;
    let worker = spawn_worker("worker-a", &controller.base_url, cache_dir, 6, true).await;
    let media = wait_for_playlist(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
    )
    .await;
    assert!(media.contains(&format!("{}/segments/live/720p/0.ts", worker.base_url)));
    // キャッシュに残した古い内容ではなく、controller から取り直した内容を配信する
    wait_for_segment(
        &client,
        &format!("{}/segments/live/720p/5.ts", worker.base_url),
        b"segment-720p-seg-5",
    )
    .await;
}

/// controller は認証トークンを設定したときだけ PUT に Authorization ヘッダーを要求する
#[tokio::test]
async fn put_requires_bearer_token_when_auth_token_is_set() {
    let tmp = TempDir::new("put-auth");
    let controller = spawn_controller_with_auth(
        any_addr(),
        tmp.path.join("storage"),
        6,
        100,
        10_000,
        "example-token",
    )
    .await;
    let client = reqwest::Client::new();
    let segment_url = format!(
        "{}/segments/live/720p/seg-0?duration_ms=6000&bandwidth=3000000&resolution=1280x720",
        controller.base_url
    );

    // トークンなしの PUT は 401 になり、セグメントファイルを保存しない
    let response = client
        .put(&segment_url)
        .body("segment-720p-seg-0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .unwrap(),
        "Bearer"
    );
    assert_eq!(response.text().await.unwrap(), "unauthorized");

    // 違うトークンの PUT も 401 になる
    let response = client
        .put(&segment_url)
        .bearer_auth("wrong-token")
        .body("segment-720p-seg-0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // 正しいトークンの PUT は成功する。番号が 0 なので、401 の PUT は番号を消費していない
    let response = client
        .put(&segment_url)
        .bearer_auth("example-token")
        .body("segment-720p-seg-0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["sequence"], 0);

    // worker の登録も同じトークンを要求する
    let register_url = format!("{}/workers/worker-a", controller.base_url);
    let response = register_worker(
        &client,
        &controller.base_url,
        "worker-a",
        "http://127.0.0.1:9001",
        1,
    )
    .await;
    assert_eq!(response.status().as_u16(), 401);
    let response = client
        .put(&register_url)
        .bearer_auth("example-token")
        .json(&json!({ "public_url": "http://127.0.0.1:9001", "weight": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);

    // 読み取りの GET はトークンなしで使える
    let response = client
        .get(format!("{}/channels/live/720p.m3u8", controller.base_url))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status().as_u16(), 401);
    controller.shutdown().await;
}

/// worker は認証トークンを設定すると登録 PUT に Authorization ヘッダーを付ける
#[tokio::test]
async fn worker_registers_with_bearer_token_and_syncs() {
    let tmp = TempDir::new("worker-auth");
    let controller = spawn_controller_with_auth(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        10_000,
        "example-token",
    )
    .await;
    let client = reqwest::Client::new();
    let worker = spawn_worker_with_auth(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
        "example-token",
    )
    .await;
    let response = publish_with_token(
        &client,
        PublishInput {
            base_url: &controller.base_url,
            quality: "720p",
            identifier: "seg-0",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: b"segment-720p-seg-0",
        },
        Some("example-token"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 201);

    // 認証下でも登録と sync が通り、worker がセグメントファイルを配信できる
    let media = wait_for_playlist(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
    )
    .await;
    assert!(media.contains(&format!("{}/segments/live/720p/0.ts", worker.base_url)));
    wait_for_segment(
        &client,
        &format!("{}/segments/live/720p/0.ts", worker.base_url),
        b"segment-720p-seg-0",
    )
    .await;
    controller.shutdown().await;
}

/// 外部配信の master m3u8 を登録する。`None` で取り消す
async fn declare_external_m3u8(
    client: &reqwest::Client,
    base_url: &str,
    external_m3u8: Option<&str>,
) -> reqwest::Response {
    client
        .put(format!("{base_url}/channels/live/external"))
        .json(&json!({ "external_m3u8": external_m3u8 }))
        .send()
        .await
        .unwrap()
}

/// リダイレクトを追わずに master playlist を取得する
async fn get_master_without_redirect(url: &str) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    client.get(url).send().await.unwrap()
}

/// 外部配信として動く別の controller を起動し、その master m3u8 の URL を返す
async fn spawn_external_origin(
    dir: &TempDir,
) -> (support::ControllerHandle, support::WorkerHandle, String) {
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        dir.path.join("storage"),
        6,
        50,
        1000,
        0,
        1,
    )
    .await;
    let worker = spawn_worker(
        "worker-external",
        &controller.base_url,
        dir.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let master_url = format!("{}/channels/live/master.m3u8", controller.base_url);
    wait_for_playlist(&client, &master_url).await;
    (controller, worker, master_url)
}

/// シェアが 100% のときは master を外部配信の master m3u8 へ転送する
#[tokio::test]
async fn redirects_master_to_external_playlist() {
    let external_dir = TempDir::new("external-origin");
    let (_external, _external_worker, external_master) = spawn_external_origin(&external_dir).await;
    let tmp = TempDir::new("redirect-external");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        100,
    )
    .await;
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let declared = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some(external_master.as_str()),
    )
    .await;
    assert_eq!(declared.status().as_u16(), 204);
    let _ = &worker;

    let master_url = format!("{}/channels/live/master.m3u8", controller.base_url);
    let response = get_master_without_redirect(&master_url).await;
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        response.headers()[reqwest::header::LOCATION],
        external_master.as_str()
    );
    // 接続元ごとの判定が中間キャッシュに固定されないようにする
    assert_eq!(
        response.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    // 転送先は実際に取得できる
    let followed = client.get(&master_url).send().await.unwrap();
    assert_eq!(followed.status().as_u16(), 200);
    assert!(followed.text().await.unwrap().contains("720p.m3u8"));

    // publisher は external=0 で自前の master を読む
    let local_url = format!("{master_url}?external=0");
    wait_for_playlist(&client, &local_url).await;
    let local = client.get(&local_url).send().await.unwrap();
    assert_eq!(local.status().as_u16(), 200);
    assert!(local.text().await.unwrap().contains("720p.m3u8"));

    controller.shutdown().await;
}

/// シェアが 0% で自前の配信が使えるときは自前の master を返す
#[tokio::test]
async fn keeps_local_master_when_external_percent_is_zero() {
    let external_dir = TempDir::new("external-origin");
    let (_external, _external_worker, external_master) = spawn_external_origin(&external_dir).await;
    let tmp = TempDir::new("local-master");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        0,
    )
    .await;
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let declared = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some(external_master.as_str()),
    )
    .await;
    assert_eq!(declared.status().as_u16(), 204);
    // 公開範囲が最新セグメントに追いつけば、遅れは 0 になり最低の割合のままになる
    wait_for_playlist_contains(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
        &format!("{}/segments/live/720p/0.ts", worker.base_url),
    )
    .await;

    let response = get_master_without_redirect(&format!(
        "{}/channels/live/master.m3u8",
        controller.base_url
    ))
    .await;
    assert_eq!(response.status().as_u16(), 200);
    // 接続元ごとに振り分ける応答は、共有キャッシュに固定されないようにする
    assert_eq!(
        response.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    assert!(response.text().await.unwrap().contains("720p.m3u8"));

    // external=1 は割合に関係なく外部配信へ振り分ける
    let forced = get_master_without_redirect(&format!(
        "{}/channels/live/master.m3u8?external=1",
        controller.base_url
    ))
    .await;
    assert_eq!(forced.status().as_u16(), 302);
    assert_eq!(
        forced.headers()[reqwest::header::LOCATION],
        external_master.as_str()
    );

    // external=0 は割合に関係なく自前の master を返す
    let local = get_master_without_redirect(&format!(
        "{}/channels/live/master.m3u8?external=0",
        controller.base_url
    ))
    .await;
    assert_eq!(local.status().as_u16(), 200);

    controller.shutdown().await;
}

/// 配信できる quality が 1 つもないときは、最低の割合が 0 でも外部配信へ逃がす
#[tokio::test]
async fn falls_back_to_external_when_no_quality_can_be_served() {
    let external_dir = TempDir::new("external-origin");
    let (_external, _external_worker, external_master) = spawn_external_origin(&external_dir).await;
    let tmp = TempDir::new("fallback-external");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        0,
    )
    .await;
    let client = reqwest::Client::new();
    // worker を起動しないため公開範囲ができず、自前では配信できない
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let declared = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some(external_master.as_str()),
    )
    .await;
    assert_eq!(declared.status().as_u16(), 204);

    let response = get_master_without_redirect(&format!(
        "{}/channels/live/master.m3u8",
        controller.base_url
    ))
    .await;
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        response.headers()[reqwest::header::LOCATION],
        external_master.as_str()
    );

    controller.shutdown().await;
}

/// controller は外部 playlist の到達確認をしないため、宣言があればそのまま転送する
#[tokio::test]
async fn routes_to_external_without_checking_reachability() {
    let tmp = TempDir::new("external-unchecked");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        100,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    // 存在しない channel を外部として登録しても、そのまま転送する
    let missing = format!("{}/channels/missing/master.m3u8", controller.base_url);
    let declared =
        declare_external_m3u8(&client, &controller.base_url, Some(missing.as_str())).await;
    assert_eq!(declared.status().as_u16(), 204);

    let response = get_master_without_redirect(&format!(
        "{}/channels/live/master.m3u8",
        controller.base_url
    ))
    .await;
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        response.headers()[reqwest::header::LOCATION],
        missing.as_str()
    );

    controller.shutdown().await;
}

/// `external_m3u8=none` で宣言を取り消すと自前の master を返す
#[tokio::test]
async fn clears_external_playlist_declaration() {
    let tmp = TempDir::new("external-clear");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        100,
    )
    .await;
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let external = "https://px.example.test/abc/def.m3u8";
    let declared = declare_external_m3u8(&client, &controller.base_url, Some(external)).await;
    assert_eq!(declared.status().as_u16(), 204);

    let master_url = format!("{}/channels/live/master.m3u8", controller.base_url);
    let response = get_master_without_redirect(&master_url).await;
    assert_eq!(response.status().as_u16(), 302);

    // null で取り消すと、以後は自前の master を返す
    let cleared = declare_external_m3u8(&client, &controller.base_url, None).await;
    assert_eq!(cleared.status().as_u16(), 204);
    wait_for_playlist_contains(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
        &format!("{}/segments/live/720p/0.ts", worker.base_url),
    )
    .await;
    let response = get_master_without_redirect(&master_url).await;
    assert_eq!(response.status().as_u16(), 200);
    assert!(response.text().await.unwrap().contains("720p.m3u8"));

    controller.shutdown().await;
}

/// 同じ client の判定はリクエストをまたいでも変わらない
#[tokio::test]
async fn keeps_the_same_route_for_the_same_client() {
    let external_dir = TempDir::new("external-origin");
    let (_external, _external_worker, external_master) = spawn_external_origin(&external_dir).await;
    let tmp = TempDir::new("sticky-route");
    let controller = spawn_controller_with_external_percent(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        0,
        50,
    )
    .await;
    let worker = spawn_worker(
        "worker-a",
        &controller.base_url,
        tmp.path.join("cache"),
        12,
        true,
    )
    .await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let declared = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some(external_master.as_str()),
    )
    .await;
    assert_eq!(declared.status().as_u16(), 204);
    wait_for_playlist_contains(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
        &format!("{}/segments/live/720p/0.ts", worker.base_url),
    )
    .await;

    let master_url = format!("{}/channels/live/master.m3u8", controller.base_url);
    let first = get_master_without_redirect(&master_url).await;
    let second = get_master_without_redirect(&master_url).await;
    assert_eq!(first.status(), second.status());

    controller.shutdown().await;
}

/// 外部配信の URL が不正な PUT は 400 を返す
#[tokio::test]
async fn rejects_invalid_external_m3u8() {
    let tmp = TempDir::new("external-invalid");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 50, 1000).await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    for value in [
        "ftp://example.test/master.m3u8",
        "/master.m3u8",
        "https://example.test/master.m3u8#fragment",
    ] {
        let response = declare_external_m3u8(&client, &controller.base_url, Some(value)).await;
        assert_eq!(response.status().as_u16(), 400, "accepted {value}");
    }
    controller.shutdown().await;
}

/// 接続情報を付けずに router を使っても、プレイリストの取得が 500 にならない
#[tokio::test]
async fn serves_playlist_without_connect_info() {
    use tower::ServiceExt;

    let tmp = TempDir::new("no-connect-info");
    tokio::fs::create_dir_all(tmp.path.join("storage"))
        .await
        .unwrap();
    let store = edamame_controller::CanonicalStore::new(tmp.path.join("storage"));
    let state = edamame_controller::ControllerState::new(store, 6, 50, 1000, 0, 1);
    let app = edamame_controller::router(state, &CorsOrigins::Disabled, None);
    let request = axum::http::Request::builder()
        .uri("/channels/live/master.m3u8")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    // 接続元が分からなくても、channel が無いことだけを理由に 404 を返す
    assert_eq!(response.status().as_u16(), 404);
}

/// channel が無いときの外部配信の登録は 404 を返す
#[tokio::test]
async fn requires_channel_to_register_external_m3u8() {
    let tmp = TempDir::new("external-no-channel");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 50, 1000).await;
    let client = reqwest::Client::new();
    let response = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some("https://px.example.test/abc/def.m3u8"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 404);
    controller.shutdown().await;
}

/// 認証を有効にすると外部配信の登録にもトークンが要る
#[tokio::test]
async fn registering_external_m3u8_requires_bearer_token() {
    let tmp = TempDir::new("external-auth");
    let controller = spawn_controller_with_auth(
        any_addr(),
        tmp.path.join("storage"),
        6,
        50,
        1000,
        "example-token",
    )
    .await;
    let client = reqwest::Client::new();
    publish_with_token(
        &client,
        PublishInput {
            base_url: &controller.base_url,
            quality: "720p",
            identifier: "seg-0",
            duration_ms: 6000,
            bandwidth: 3_000_000,
            resolution: "1280x720",
            body: b"segment-720p-seg-0",
        },
        Some("example-token"),
    )
    .await;

    // トークンが無い登録は 401 になる
    let response = declare_external_m3u8(
        &client,
        &controller.base_url,
        Some("https://px.example.test/abc/def.m3u8"),
    )
    .await;
    assert_eq!(response.status().as_u16(), 401);

    // トークンを付ければ登録できる
    let response = client
        .put(format!("{}/channels/live/external", controller.base_url))
        .bearer_auth("example-token")
        .json(&json!({ "external_m3u8": "https://px.example.test/abc/def.m3u8" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);
    controller.shutdown().await;
}

/// `external` に 0 と 1 以外を指定すると 400 を返す
#[tokio::test]
async fn rejects_invalid_external_query() {
    let tmp = TempDir::new("external-switch-invalid");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 50, 1000).await;
    let client = reqwest::Client::new();
    publish_segment(&client, &controller.base_url, "720p", "seg-0").await;
    let response = client
        .get(format!(
            "{}/channels/live/master.m3u8?external=2",
            controller.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    controller.shutdown().await;
}
