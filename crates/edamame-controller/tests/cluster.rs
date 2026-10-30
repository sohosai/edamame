//! 実際の controller プロセスと worker で選出、転送、状態の破棄を確認する。

mod support;

use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::{Value, json};
use support::{
    available_addr, spawn_cluster_process, spawn_worker_with_auth, wait_for_playlist,
    wait_for_role, wait_for_segment,
};

const TOKEN: &str = "cluster-test-token";

/// 中継後の採番をテストで比較できるよう、認証付き PUT のセグメント番号を返す。
async fn publish(client: &Client, base: &str, identifier: &str, body: &str) -> u64 {
    let response = client
        .put(format!(
            "{base}/segments/live/720p/{identifier}?duration_ms=6000&bandwidth=3000000"
        ))
        .bearer_auth(TOKEN)
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "セグメントを保存できなかった"
    );
    response.json::<Value>().await.unwrap()["sequence"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn forwards_requests_and_discards_state_on_failover_and_failback() {
    let client = Client::builder().redirect(Policy::none()).build().unwrap();
    let following = Client::new();
    let low_addr = available_addr();
    let high_addr = available_addr();
    let low_url = format!("http://{low_addr}");
    let high_url = format!("http://{high_addr}");
    // seed なしの機体でも、後から接触した peer を選出に取り込む。
    let low = spawn_cluster_process(low_addr, 100, &[], "").await;
    wait_for_role(&client, &low_url, "leader", &low_url).await;
    assert_eq!(publish(&client, &low_url, "before-0", "old-0").await, 0);
    assert_eq!(publish(&client, &low_url, "before-1", "old-1").await, 1);
    let mut high = spawn_cluster_process(high_addr, 50, std::slice::from_ref(&low_url), "").await;
    wait_for_role(&client, &high_url, "leader", &high_url).await;
    wait_for_role(&client, &low_url, "follower", &high_url).await;
    let health: Value = client
        .get(format!("{low_url}/internal/cluster/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["public_url"], low_url);
    assert_eq!(health["metric"], 100);

    // follower の認証を通過し、採番と保存は leader が行う。
    assert_eq!(publish(&client, &low_url, "first", "first-body").await, 0);
    assert_eq!(
        publish(&client, &high_url, "second", "second-body").await,
        1
    );
    let unauthorized = client
        .put(format!("{low_url}/workers/anonymous"))
        .json(&json!({"public_url":"http://127.0.0.1:1","weight":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let external = client
        .put(format!("{low_url}/channels/live/external"))
        .bearer_auth(TOKEN)
        .json(&json!({"external_m3u8":"https://example.test/live.m3u8"}))
        .send()
        .await
        .unwrap();
    assert_eq!(external.status(), StatusCode::NO_CONTENT);
    let missing = client
        .put(format!("{low_url}/channels/missing/external"))
        .bearer_auth(TOKEN)
        .json(&json!({"external_m3u8":null}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing.text().await.unwrap(), "channel not found");

    // query と CORS を含む redirect を client が追従できる。
    let segment_url = format!("{low_url}/segments/live/720p/0.ts?x=1%2F2&y=3");
    let redirect = client
        .get(&segment_url)
        .header("Origin", "https://player.test")
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        redirect.headers()["location"],
        format!("{high_url}/segments/live/720p/0.ts?x=1%2F2&y=3")
    );
    assert_eq!(redirect.headers()["cache-control"], "no-store");
    assert_eq!(redirect.headers()["access-control-allow-origin"], "*");
    let segment = following.get(&segment_url).send().await.unwrap();
    assert_eq!(segment.status(), StatusCode::OK);
    assert_eq!(segment.text().await.unwrap(), "first-body");
    let playlist = client
        .get(format!("{low_url}/channels/live/master.m3u8?external=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(playlist.status(), StatusCode::TEMPORARY_REDIRECT);

    let registration = client
        .put(format!("{low_url}/workers/manual"))
        .bearer_auth(TOKEN)
        .json(&json!({"public_url":"http://127.0.0.1:1", "weight":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(registration.status(), StatusCode::CREATED);
    assert_eq!(
        registration.json::<Value>().await.unwrap()["sync_interval_ms"],
        100
    );
    let sync_url = format!("{low_url}/workers/manual/sync");
    let sync_body = json!({"cached_segments":[], "fetching_segments":[]});
    let redirect = client
        .post(&sync_url)
        .json(&sync_body)
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        redirect.headers()["location"],
        format!("{high_url}/workers/manual/sync")
    );
    assert_eq!(redirect.headers()["cache-control"], "no-store");
    let sync = following
        .post(&sync_url)
        .json(&sync_body)
        .send()
        .await
        .unwrap();
    assert_eq!(sync.status(), StatusCode::OK);
    assert_eq!(
        sync.json::<Value>().await.unwrap()["fetch_segments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let unauthorized = client
        .post(format!("{low_url}/internal/cluster/heartbeat"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let wrong_token = client
        .post(format!("{low_url}/internal/cluster/heartbeat"))
        .bearer_auth("wrong-token")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

    let worker = spawn_worker_with_auth("real-worker", &low_url, 12, true, TOKEN).await;
    let worker_segment = format!("{}/segments/live/720p/0.ts", worker.base_url);
    wait_for_segment(&following, &worker_segment, b"first-body").await;
    let playlist_url = format!("{low_url}/channels/live/720p.m3u8");
    let playlist = wait_for_playlist(&following, &playlist_url).await;
    assert!(
        playlist.contains(&worker.base_url),
        "実 worker が配信先にならなかった"
    );

    // 低優先度機が前の leader 時代のデータを捨てたことも確認する。
    high.child.kill().await.unwrap();
    let failed_relay = client
        .put(format!(
            "{low_url}/segments/live/720p/lost?duration_ms=6000&bandwidth=3000000"
        ))
        .bearer_auth(TOKEN)
        .body("lost")
        .send()
        .await
        .unwrap();
    assert_eq!(failed_relay.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed_relay.headers()["retry-after"], "1");
    wait_for_role(&client, &low_url, "leader", &low_url).await;
    let unknown = client
        .post(&sync_url)
        .json(&sync_body)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let discarded = client
        .get(format!("{low_url}/segments/live/720p/1.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        discarded.status(),
        StatusCode::NOT_FOUND,
        "降格前のセグメントが残っていた"
    );
    assert_eq!(
        publish(&client, &low_url, "after-failure", "failover-body").await,
        0
    );
    assert_eq!(
        publish(&client, &low_url, "after-failure-1", "failover-body-1").await,
        1
    );
    wait_for_segment(&following, &worker_segment, b"failover-body").await;
    wait_for_playlist(&following, &playlist_url).await;

    // 上位機の復帰で戻し、直前の採番と worker のキャッシュが失われる。
    let mut high = spawn_cluster_process(high_addr, 50, std::slice::from_ref(&low_url), "").await;
    wait_for_role(&client, &high_url, "leader", &high_url).await;
    wait_for_role(&client, &low_url, "follower", &high_url).await;
    let absent = following
        .get(format!("{low_url}/segments/live/720p/1.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        publish(&client, &low_url, "after-return", "failback-body").await,
        0
    );
    wait_for_segment(&following, &worker_segment, b"failback-body").await;
    wait_for_playlist(&following, &playlist_url).await;
    high.child.kill().await.unwrap();
    wait_for_role(&client, &low_url, "leader", &low_url).await;
    assert_eq!(
        publish(&client, &low_url, "after-second-failure", "fresh-body").await,
        0
    );
    drop(low);
}

#[tokio::test]
async fn discovers_members_transitively_and_supports_url_prefixes() {
    let client = Client::builder().redirect(Policy::none()).build().unwrap();
    let a = available_addr();
    let b = available_addr();
    let c = available_addr();
    let a_url = format!("http://{a}/a");
    let b_url = format!("http://{b}/b");
    let c_url = format!("http://{c}/c");
    let _a = spawn_cluster_process(a, 30, std::slice::from_ref(&b_url), "/a").await;
    let mut middle = spawn_cluster_process(b, 20, std::slice::from_ref(&a_url), "/b").await;
    let _c = spawn_cluster_process(c, 10, std::slice::from_ref(&b_url), "/c").await;
    wait_for_role(&client, &a_url, "follower", &c_url).await;
    wait_for_role(&client, &b_url, "follower", &c_url).await;
    wait_for_role(&client, &c_url, "leader", &c_url).await;
    assert_eq!(publish(&client, &a_url, "prefixed", "prefix-body").await, 0);
    let redirect = client
        .get(format!("{a_url}/segments/live/720p/0.ts?q=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        redirect.headers()["location"],
        format!("{c_url}/segments/live/720p/0.ts?q=1")
    );
    // 中継した peer が死んでも、発見済みの member とは直接 heartbeat を続ける。
    middle.child.kill().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    wait_for_role(&client, &a_url, "follower", &c_url).await;
    assert_eq!(publish(&client, &a_url, "direct", "direct-body").await, 1);
}

#[tokio::test]
async fn slow_upload_does_not_block_heartbeats_or_write_after_demotion() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let client = Client::new();
    let low_addr = available_addr();
    let high_addr = available_addr();
    let low_url = format!("http://{low_addr}");
    let high_url = format!("http://{high_addr}");
    let _low = spawn_cluster_process(low_addr, 20, std::slice::from_ref(&high_url), "").await;
    wait_for_role(&client, &low_url, "leader", &low_url).await;
    let mut upload = tokio::net::TcpStream::connect(low_addr).await.unwrap();
    upload.write_all(format!("PUT /segments/live/720p/slow?duration_ms=6000&bandwidth=3000000 HTTP/1.1\r\nHost: {low_addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 4\r\nConnection: close\r\n\r\na").as_bytes()).await.unwrap();
    // body を送り切る前でも、上位機を認識して降格できる必要がある。
    let mut high = spawn_cluster_process(high_addr, 10, std::slice::from_ref(&low_url), "").await;
    wait_for_role(&client, &low_url, "follower", &high_url).await;
    wait_for_role(&client, &high_url, "leader", &high_url).await;
    upload.write_all(b"bcd").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        upload.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 201"),
        "受信完了後に新しい leader へ中継できなかった"
    );
    let body = client
        .get(format!("{high_url}/segments/live/720p/0.ts"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "abcd");
    high.child.kill().await.unwrap();
    wait_for_role(&client, &low_url, "leader", &low_url).await;
    assert_eq!(publish(&client, &low_url, "fresh", "fresh").await, 0);
}
