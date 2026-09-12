//! controller と worker の HTTP 契約と統合動作を実サーバーで確認する

mod support;

use std::time::Duration;

use edamame_domain::CorsOrigins;
use serde_json::{Value, json};
use support::{
    TempDir, spawn_controller, spawn_controller_with_cors, spawn_worker, spawn_worker_with_cors,
    wait_for_playlist, wait_for_segment,
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
    client
        .put(format!(
            "{}/segments/live/{}/{}?duration_ms={}&bandwidth={}&resolution={}",
            input.base_url,
            input.quality,
            input.identifier,
            input.duration_ms,
            input.bandwidth,
            input.resolution
        ))
        .body(input.body.to_vec())
        .send()
        .await
        .unwrap()
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
) -> reqwest::Response {
    client
        .put(format!("{base_url}/workers/{worker_id}"))
        .json(&json!({ "public_url": public_url }))
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
    let registration = register_worker(&client, base, "worker-a", worker_public_url).await;
    assert_eq!(registration.status().as_u16(), 201);
    let registration_body: Value = registration.json().await.unwrap();
    assert_eq!(registration_body["sync_interval_ms"], 1000);

    let sync = sync_worker(&client, base, "worker-a", cached("720p", sequences)).await;
    assert_eq!(sync.status().as_u16(), 200);
    let sync_body: Value = sync.json().await.unwrap();
    assert_eq!(sync_body["state"], "READY");
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
    assert_eq!(gap.status().as_u16(), 422);

    register_worker(&client, base, "worker-a", "http://worker.test").await;
    let sync = sync_worker(&client, base, "worker-a", cached("720p", 0..3)).await;
    assert_eq!(sync.status().as_u16(), 200);
    let gap_with_worker = client
        .get(format!("{base}/channels/live/480p.m3u8"))
        .send()
        .await
        .unwrap();
    assert_eq!(gap_with_worker.status().as_u16(), 422);

    // 欠けた識別子は、別の quality が先に割り当てた番号を使う
    assert_eq!(publish_segment(&client, base, "480p", "seg-1").await, 1);
    let mut cached_segments = cached("720p", 0..3);
    cached_segments.extend(cached("480p", 0..3));
    let sync = sync_worker(&client, base, "worker-a", cached_segments).await;
    let sync_body: Value = sync.json().await.unwrap();
    assert_eq!(sync_body["state"], "READY");
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

    register_worker(&client, base, "worker-a", "http://worker.test").await;
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

/// プレイリストはリクエストのたびに現在のセグメントから作り、古い範囲を返さない
#[tokio::test]
async fn media_playlist_reflects_new_segment() {
    let tmp = TempDir::new("no-stale");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test").await;
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

#[tokio::test]
async fn file_deleted_in_range_returns_422() {
    let tmp = TempDir::new("deleted");
    let storage = tmp.path.join("storage");
    let controller = spawn_controller(any_addr(), storage.clone(), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test").await;
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

#[tokio::test]
async fn worker_registration_and_sync_contract() {
    let tmp = TempDir::new("worker-contract");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    publish_series(&client, base, "720p", 0..6).await;
    let created = register_worker(&client, base, "worker-a", "http://worker.test").await;
    assert_eq!(created.status().as_u16(), 201);
    let updated = register_worker(&client, base, "worker-a", "http://worker.test").await;
    assert_eq!(updated.status().as_u16(), 200);
    let preparing = sync_worker(&client, base, "worker-a", cached("720p", 0..5)).await;
    let preparing_body: Value = preparing.json().await.unwrap();
    assert_eq!(preparing_body["state"], "PREPARING");
    assert_eq!(
        preparing_body["fetch_segments"],
        json!([{ "channel_id": "live", "quality": "720p", "sequence": 5 }])
    );
    let ready = sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    let ready_body: Value = ready.json().await.unwrap();
    assert_eq!(ready_body["state"], "READY");
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
        .json(&json!({ "public_url": 1 }))
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
    register_worker(&client, base, "worker-a", "http://worker.test").await;
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
    register_worker(&client, base, "worker-a", "http://worker.test").await;
    sync_worker(&client, base, "worker-a", cached("720p", 0..6)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let updated = register_worker(&client, base, "worker-a", "http://worker.test").await;
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

/// 登録を更新すると報告済みのキャッシュを無効にし、READY の worker を選ばなくなる
#[tokio::test]
async fn registration_resets_cached_segments() {
    let tmp = TempDir::new("registration-reset");
    let controller = spawn_controller(any_addr(), tmp.path.join("storage"), 6, 1000, 10000).await;
    let client = reqwest::Client::new();
    let base = &controller.base_url;
    let sequences = publish_series(&client, base, "720p", 0..6).await;
    register_worker(&client, base, "worker-a", "http://worker.test").await;
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

    // worker は登録のたびにキャッシュを捨てるため、登録を更新した時点で READY ではなくなる
    let updated = register_worker(&client, base, "worker-a", "http://worker.test").await;
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

    // 次の sync で報告し直せば READY に戻る
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
    let media = wait_for_playlist(
        &client,
        &format!("{}/channels/live/720p.m3u8", controller.base_url),
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
