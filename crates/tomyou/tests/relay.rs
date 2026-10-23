//! 実 ffmpeg と GStreamer を使った RTMP 中継の結合テスト

use std::{process::Stdio, time::Duration};

use tokio::process::Command;
use tomyou::{
    gstreamer,
    relay::{self, RelayEvent},
};

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_accepts_readers_without_flash_version() {
    let Ok(launch) = gstreamer::resolve(None) else {
        eprintln!("GStreamer がないため、接続情報を省略する検証を省略します");
        return;
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = relay::start(
        "127.0.0.1:0".parse().expect("待ち受けアドレスを解釈できる"),
        "live",
        "uni",
        tx,
    )
    .await
    .expect("RTMP 中継を起動できる");
    let address = match rx.recv().await {
        Some(RelayEvent::Listening(address)) => address,
        event => panic!("待ち受け開始の通知を受信できません: {event:?}"),
    };

    // librtmp の URL 再利用で flashVer が消える場合も、同じ中継に再び接続できる必要がある。
    for _ in 0..2 {
        let mut reader = Command::new(&launch)
            .args([
                "-e",
                "rtmpsrc",
                &format!("location=rtmp://{address}/live/uni live=1"),
                "!",
                "fakesink",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("flashVer を省略した GStreamer を起動できる");
        let connected = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, RelayEvent::PlayerConnected) {
                    return true;
                }
            }
            false
        })
        .await;
        reader
            .kill()
            .await
            .expect("検証用の GStreamer を終了できる");
        let output = reader
            .wait_with_output()
            .await
            .expect("GStreamer の終了結果を読める");
        assert!(
            matches!(connected, Ok(true)),
            "flashVer を省略した接続に失敗しました: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    handle.stop();
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
            "6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "60",
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

    let late_path = std::env::temp_dir().join(format!(
        "tomyou-late-player-{}-{}.flv",
        std::process::id(),
        address.port()
    ));
    let late_player = if let Ok(launch) = gstreamer::resolve(None) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events_rx.recv().await {
                    Some(RelayEvent::PublisherConnected) => break,
                    Some(_) => {}
                    None => panic!("配信側の接続通知が終了しました"),
                }
            }
        })
        .await
        .expect("配信側が接続する");
        // 最初のキーフレーム後に接続し、次のキーフレームより先に 1 秒以上の音声が届く状態を作る。
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut spec = gstreamer::edamame_spec(&url, "rtmp://127.0.0.1/unused");
        spec.args.insert(3, "num-buffers=8".to_owned());
        let end = spec.args.len();
        // 送信先の接続条件から独立して、実際の分離、解析、再多重化までを検証する。
        spec.args[end - 2] = "filesink".to_owned();
        spec.args[end - 1] = format!("location={}", late_path.display());
        Some(
            Command::new(launch)
                .args(spec.args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("途中接続する GStreamer を起動できる"),
        )
    } else {
        eprintln!("GStreamer がないため、GStreamer の受信検証を省略します");
        None
    };

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

    if let Some(player) = late_player {
        let output = tokio::time::timeout(Duration::from_secs(10), player.wait_with_output())
            .await
            .expect("途中接続する GStreamer が時間内に終了する")
            .expect("途中接続する GStreamer の終了結果を取得できる");
        assert!(
            output.status.success(),
            "途中接続の受信に失敗しました: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_packets",
                "-show_entries",
                "stream=codec_name,nb_read_packets",
                "-of",
                "json",
            ])
            .arg(&late_path)
            .output()
            .await
            .expect("受信した音声と映像を検査できる");
        assert!(
            probe.status.success(),
            "受信した FLV を解析できません: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let data: serde_json::Value =
            serde_json::from_slice(&probe.stdout).expect("解析結果を読める");
        let streams = data["streams"]
            .as_array()
            .expect("ストリームの一覧を読める");
        for codec in ["h264", "aac"] {
            assert!(
                streams.iter().any(|stream| stream["codec_name"] == codec
                    && stream["nb_read_packets"]
                        .as_str()
                        .and_then(|n| n.parse::<u64>().ok())
                        .is_some_and(|n| n > 0)),
                "{codec} のパケットを受信できません: {data}"
            );
        }
        std::fs::remove_file(&late_path).expect("検証用の FLV を削除できる");
    }

    handle.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_keeps_reader_connected_across_ack_windows() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg がないため、ACK を伴う連続受信の検証を省略します");
        return;
    }
    let Ok(launch) = gstreamer::resolve(None) else {
        eprintln!("GStreamer がないため、ACK を伴う連続受信の検証を省略します");
        return;
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = relay::start(
        "127.0.0.1:0".parse().expect("待ち受けアドレスを解釈できる"),
        "live",
        "uni",
        tx,
    )
    .await
    .expect("RTMP 中継を起動できる");
    let address = match rx.recv().await {
        Some(RelayEvent::Listening(address)) => address,
        event => panic!("待ち受け開始の通知を受信できません: {event:?}"),
    };
    let url = format!("rtmp://{address}/live/uni");
    let path = std::env::temp_dir().join(format!(
        "tomyou-ack-{}-{}.flv",
        std::process::id(),
        address.port()
    ));
    let mut reader = Command::new(launch)
        .args([
            "-e",
            "rtmpsrc",
            &format!("location={url} live=1"),
            "!",
            "filesink",
            &format!("location={}", path.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("受信側の GStreamer を起動できる");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(RelayEvent::PlayerConnected) => break,
                Some(_) => {}
                None => panic!("再生側の接続通知が終了しました"),
            }
        }
    })
    .await
    .expect("再生側が接続する");

    // 短い低ビットレートの配信では最初の ACK まで届かないため、確認応答の間隔を超える量を実送信する。
    let publisher = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000",
            "-t",
            "8",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "30",
            "-b:v",
            "16M",
            "-minrate",
            "16M",
            "-maxrate",
            "16M",
            "-bufsize",
            "16M",
            "-x264-params",
            "nal-hrd=cbr",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "flv",
            &url,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("高ビットレートの配信を起動できる");
    let published = tokio::time::timeout(Duration::from_secs(15), publisher.wait_with_output())
        .await
        .expect("配信が時間内に終了する")
        .expect("配信の終了結果を読める");
    assert!(
        published.status.success(),
        "配信に失敗しました: {}",
        String::from_utf8_lossy(&published.stderr)
    );
    let mut disconnected = false;
    while let Ok(event) = rx.try_recv() {
        disconnected |= matches!(event, RelayEvent::PlayerDisconnected);
    }
    let bytes = std::fs::metadata(&path)
        .expect("受信ファイルの大きさを取得できる")
        .len();
    let _ = reader.kill().await;
    let output = reader
        .wait_with_output()
        .await
        .expect("受信の終了結果を読める");
    handle.stop();
    std::fs::remove_file(path).expect("検証用の FLV を削除できる");
    assert!(
        !disconnected && bytes > 10_000_000,
        "ACK の間隔を超えて受信を継続できません: 受信バイト数={bytes}, 切断={disconnected}, 詳細={}",
        String::from_utf8_lossy(&output.stderr)
    );
}
