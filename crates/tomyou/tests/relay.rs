//! 実 ffmpeg を使った RTMP 中継の結合テスト

use std::{process::Stdio, time::Duration};

use tokio::process::Command;
use tomyou::relay::{self, RelayEvent};

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_rtmp_to_player() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg がないため、RTMP 中継の結合テストを省略します");
        return;
    }

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = relay::start(
        "127.0.0.1:0".parse().expect("待ち受けアドレス"),
        "live",
        "uni",
        events_tx,
    )
    .await
    .expect("RTMP 中継を開始できる");

    let address = loop {
        match events_rx.recv().await {
            Some(RelayEvent::Listening(address)) => break address,
            Some(_) => {}
            None => panic!("RTMP 中継の待ち受けアドレスを取得できない"),
        }
    };
    let url = format!("rtmp://{address}/live/uni");

    let mut player = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &url,
            "-t",
            "2",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("再生側の ffmpeg を起動できる");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut publisher = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "30",
            "-c:a",
            "aac",
            "-f",
            "flv",
            &url,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("配信側の ffmpeg を起動できる");

    let publisher_status = tokio::time::timeout(Duration::from_secs(15), publisher.wait())
        .await
        .expect("配信側が時間内に終了する")
        .expect("配信側の終了状態を取得できる");
    assert!(
        publisher_status.success(),
        "配信側の ffmpeg が失敗した: {publisher_status}"
    );

    let player_status = tokio::time::timeout(Duration::from_secs(15), player.wait())
        .await
        .expect("再生側が時間内に終了する")
        .expect("再生側の終了状態を取得できる");
    assert!(
        player_status.success(),
        "再生側の ffmpeg が失敗した: {player_status}"
    );

    handle.stop();
}
