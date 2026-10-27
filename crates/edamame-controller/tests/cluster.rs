//! leader なしの controller がローカル処理と worker registry の共有を行うことを確認する。

mod support;

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use support::{
    available_addr, spawn_cluster_process, spawn_worker_with_auth,
    spawn_worker_with_auth_and_controllers, wait_for_member, wait_for_segment,
};

const TOKEN: &str = "cluster-test-token";

/// controller にセグメントを保存し、割り当てられた番号を返す。
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
        reqwest::StatusCode::CREATED,
        "セグメントを保存できなかった"
    );
    response.json::<Value>().await.unwrap()["sequence"]
        .as_u64()
        .unwrap()
}

/// worker が配信先として playlist に現れるまで待つ。
async fn wait_for_playlist_containing(client: &Client, url: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let response = client.get(url).send().await.unwrap();
        if response.status().is_success() {
            let body = response.text().await.unwrap();
            if body.contains(needle) {
                return body;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "playlist に worker が現れなかった"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn controllers_share_worker_registry_without_leader() {
    let client = Client::new();
    let a_addr = available_addr();
    let b_addr = available_addr();
    let a_url = format!("http://{a_addr}");
    let b_url = format!("http://{b_addr}");
    let _a = spawn_cluster_process(a_addr, &[], "").await;
    let _b = spawn_cluster_process(b_addr, std::slice::from_ref(&a_url), "").await;
    wait_for_member(&client, &b_url, &a_url).await;

    // leader は存在せず、どちらの controller もローカルで PUT / GET を処理する。
    let health: Value = client
        .get(format!("{a_url}/internal/cluster/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(health.get("role").is_none(), "role は返さない");
    assert!(health.get("leader_url").is_none(), "leader_url は返さない");
    assert_eq!(publish(&client, &a_url, "a-0", "same-body").await, 0);
    assert_eq!(publish(&client, &b_url, "b-0", "same-body").await, 0);
    let direct = client
        .get(format!("{b_url}/segments/live/720p/0.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(direct.status(), reqwest::StatusCode::OK);
    assert_eq!(direct.text().await.unwrap(), "same-body");

    // 認証なしの worker 登録と heartbeat は拒否する。
    let unauthorized = client
        .put(format!("{a_url}/workers/anonymous"))
        .json(&serde_json::json!({"public_url":"http://127.0.0.1:1","weight":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unauthorized = client
        .post(format!("{a_url}/internal/cluster/heartbeat"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    // A に登録した worker が A から segment を取得し、A の playlist に現れる。
    let worker = spawn_worker_with_auth("shared-worker", &a_url, 12, true, TOKEN).await;
    let worker_segment = format!("{}/segments/live/720p/0.ts", worker.base_url);
    wait_for_segment(&client, &worker_segment, b"same-body").await;
    let a_playlist = format!("{a_url}/channels/live/720p.m3u8");
    wait_for_playlist_containing(&client, &a_playlist, &worker.base_url).await;

    // worker registry は heartbeat で B にも共有され、B の playlist でも同じ worker が使われる。
    let b_playlist = format!("{b_url}/channels/live/720p.m3u8");
    let playlist = wait_for_playlist_containing(&client, &b_playlist, &worker.base_url).await;
    assert!(
        playlist.contains(&worker.base_url),
        "B の playlist に共有された worker が現れなかった"
    );
}

#[tokio::test]
async fn discovers_members_transitively_and_supports_url_prefixes() {
    let client = Client::new();
    let a = available_addr();
    let b = available_addr();
    let c = available_addr();
    let a_url = format!("http://{a}/a");
    let b_url = format!("http://{b}/b");
    let c_url = format!("http://{c}/c");
    let _a = spawn_cluster_process(a, std::slice::from_ref(&b_url), "/a").await;
    let _b = spawn_cluster_process(b, std::slice::from_ref(&a_url), "/b").await;
    let _c = spawn_cluster_process(c, std::slice::from_ref(&b_url), "/c").await;
    wait_for_member(&client, &a_url, &b_url).await;
    wait_for_member(&client, &b_url, &c_url).await;

    // 接頭辞付きの URL でも、各 controller がローカルでセグメントを保存・取得する。
    assert_eq!(publish(&client, &a_url, "prefixed-a", "a-body").await, 0);
    assert_eq!(publish(&client, &c_url, "prefixed-c", "c-body").await, 0);
    let from_a = client
        .get(format!("{a_url}/segments/live/720p/0.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(from_a.status(), reqwest::StatusCode::OK);
    assert_eq!(from_a.text().await.unwrap(), "a-body");
    let from_c = client
        .get(format!("{c_url}/segments/live/720p/0.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(from_c.status(), reqwest::StatusCode::OK);
    assert_eq!(from_c.text().await.unwrap(), "c-body");
}

#[tokio::test]
async fn worker_switches_controller_when_current_controller_stops() {
    let client = Client::new();
    let a_addr = available_addr();
    let b_addr = available_addr();
    let a_url = format!("http://{a_addr}");
    let b_url = format!("http://{b_addr}");
    let mut a = spawn_cluster_process(a_addr, &[], "").await;
    let _b = spawn_cluster_process(b_addr, std::slice::from_ref(&a_url), "").await;
    wait_for_member(&client, &b_url, &a_url).await;

    assert_eq!(publish(&client, &a_url, "a-0", "same-body").await, 0);
    assert_eq!(publish(&client, &b_url, "b-0", "same-body").await, 0);

    // worker は A を優先し、A が停止したら B へ切り替える
    let worker = spawn_worker_with_auth_and_controllers(
        "failover-worker",
        vec![a_url.clone(), b_url.clone()],
        12,
        true,
        TOKEN,
    )
    .await;
    let worker_segment = format!("{}/segments/live/720p/0.ts", worker.base_url);
    wait_for_segment(&client, &worker_segment, b"same-body").await;
    let a_playlist = format!("{a_url}/channels/live/720p.m3u8");
    wait_for_playlist_containing(&client, &a_playlist, &worker.base_url).await;

    a.child.kill().await.unwrap();
    let b_playlist = format!("{b_url}/channels/live/720p.m3u8");
    let playlist = wait_for_playlist_containing(&client, &b_playlist, &worker.base_url).await;
    assert!(
        playlist.contains(&worker.base_url),
        "切替先の B の playlist に worker が現れなかった"
    );
    wait_for_segment(&client, &worker_segment, b"same-body").await;
}
