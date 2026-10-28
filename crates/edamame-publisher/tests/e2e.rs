//! 実プロセスを使った結合テスト
//!
//! controller、worker、ffmpeg、publisher をすべて起動し、暗号化したセグメントが controller に保存され、再生できることを確認する。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use common::{
    Controller, CredentialsServer, Publisher, Sender, TestDir, Worker, assert_mpegts,
    decrypt_segment, free_port, http_get, wait_for,
};

/// 設定ファイルを書く
fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("publisher.toml");
    fs::write(&path, text).expect("設定を書けません");
    path
}

/// 1 つの channel の設定を組み立てる
fn input_config(channel_id: &str, srt_port: u16, copy: bool) -> String {
    if copy {
        format!(
            r#"
[[input]]
channel_id = "{channel_id}"
input_url = "srt://127.0.0.1:{srt_port}?mode=listener&latency=200000"

[[input.rendition]]
quality = "main"
mode = "copy"
resolution = "640x360"
bandwidth = 12000000
"#
        )
    } else {
        format!(
            r#"
[[input]]
channel_id = "{channel_id}"
input_url = "srt://127.0.0.1:{srt_port}?mode=listener&latency=200000"

[[input.rendition]]
quality = "main"
resolution = "640x360"
bandwidth = 1400000
video_bitrate = 800000
"#
        )
    }
}

/// 復号したセグメントを ffprobe で調べる
fn probe_streams(dir: &Path, data: &[u8]) -> String {
    let path = dir.join("probe.ts");
    fs::write(&path, data).expect("セグメントを書けません");
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name,width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("ffprobe を実行できません");
    assert!(
        output.status.success(),
        "ffprobe が失敗しました: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// プレイリストのメディアシーケンスと、セグメントごとの IV を取り出す
///
/// `#EXT-X-KEY` はセグメントごとに並ぶため、n 番目の IV はメディアシーケンス + n のセグメントのものである。
fn playlist_ivs(playlist: &str) -> (u64, Vec<String>) {
    let mut media_sequence = 0;
    let mut ivs = Vec::new();
    for line in playlist.lines() {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.parse().expect("メディアシーケンスを読めません");
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:METHOD=AES-128,URI=\"") {
            let (_, iv) = value.rsplit_once(",IV=0x").expect("IV がありません");
            ivs.push(iv.to_owned());
        }
    }
    (media_sequence, ivs)
}

#[test]
fn publishes_encrypted_segments_for_all_renditions() {
    let dir = TestDir::new("renditions");
    let srt_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
[[input.rendition]]
quality = "small"
resolution = "320x180"
bandwidth = 1400000
video_bitrate = 500000
"#,
            input_config("1A", srt_port, true)
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    let _sender = Sender::start(srt_port, "640x360", 30, 30, "aac", 60);

    let stored = wait_for(
        || {
            controller.sequences("1A", "main").len() >= 2
                && controller.sequences("1A", "small").len() >= 2
        },
        Duration::from_secs(30),
    );
    assert!(stored, "セグメントが保存されません: {}", publisher.log());
    // 送信中のセットの送信が終わるのを待つ
    thread::sleep(Duration::from_millis(500));

    let main = controller.sequences("1A", "main");
    let small = controller.sequences("1A", "small");
    assert_eq!(
        main, small,
        "同じ識別子で送ったセグメントの番号が quality をまたいで揃っていません"
    );

    let key = publisher.key();
    // 暗号鍵の URI を指定しない場合は controller を介して IV を伝えられないため、全ゼロ固定になる
    let iv_zero = "0".repeat(32);
    for (quality, resolution) in [("main", "640,360"), ("small", "320,180")] {
        let sequence = main[0];
        let stored = controller
            .segment("1A", quality, sequence)
            .expect("セグメントを読めません");
        let plain = decrypt_segment(&key, &iv_zero, &stored);
        // 暗号文の先頭バイトは同期バイトと同じ値になり得るため、平文との比較で確かめる
        assert_ne!(stored, plain, "保存されたファイルが暗号化されていません");
        assert_mpegts(&plain);
        let streams = probe_streams(dir.path(), &plain);
        assert!(
            streams.contains("h264") && streams.contains(resolution) && streams.contains("aac"),
            "復号したセグメントの内容が違います: {streams}"
        );
    }

    // worker がセグメントを取得して公開範囲ができるまで待つ
    let master_url = format!("{}/channels/1A/master.m3u8", controller.base_url);
    let mut master = String::new();
    let published = wait_for(
        || match http_get(&master_url) {
            Some(body) => {
                master = body;
                true
            }
            None => false,
        },
        Duration::from_secs(20),
    );
    assert!(
        published,
        "master playlist を取得できません: {}",
        publisher.log()
    );
    assert!(
        master.contains("main.m3u8"),
        "master playlist の内容: {master}"
    );
    assert!(
        master.contains("small.m3u8"),
        "master playlist の内容: {master}"
    );
    let media_url = format!("{}/channels/1A/main.m3u8", controller.base_url);
    let media = wait_for(
        || http_get(&media_url).is_some_and(|body| body.contains("EXTINF:1.000")),
        Duration::from_secs(20),
    );
    assert!(
        media,
        "media playlist を取得できません: {}",
        publisher.log()
    );

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
    assert!(
        publisher.log().contains("stopped"),
        "停止のログが出ていません: {}",
        publisher.log()
    );
}

/// 暗号鍵の URI とセグメントごとの乱数 IV がプレイリストに反映され、その IV で復号できる
#[test]
fn publishes_key_uri_and_iv() {
    let dir = TestDir::new("key-uri");
    let srt_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let key_uri = "https://live.example.test/key/1A";
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"

[[input]]
channel_id = "1A"
input_url = "srt://127.0.0.1:{srt_port}?mode=listener&latency=200000"
key_uri = "{key_uri}"

[[input.rendition]]
quality = "main"
mode = "copy"
resolution = "640x360"
bandwidth = 12000000
"#
        ),
    );
    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    let _sender = Sender::start(srt_port, "640x360", 30, 30, "aac", 60);

    let media_url = format!("{}/channels/1A/main.m3u8", controller.base_url);
    let expected = format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\",IV=0x");
    let listed = wait_for(
        // IV の比較には 2 個必要なので、最初の 1 個だけで待機を終えない。
        || {
            http_get(&media_url)
                .is_some_and(|body| body.contains(&expected) && playlist_ivs(&body).1.len() >= 2)
        },
        Duration::from_secs(30),
    );
    assert!(
        listed,
        "鍵 URI と IV がプレイリストに出ません: {}",
        publisher.log()
    );

    let playlist = http_get(&media_url).expect("プレイリストを取得できません");
    let (first_sequence, ivs) = playlist_ivs(&playlist);
    assert!(ivs.len() >= 2, "鍵のタグが足りません: {playlist}");
    assert_ne!(ivs[0], ivs[1], "IV が使い回されています: {playlist}");

    // プレイリストが示す IV で、保存されたセグメントを復号できる
    let stored = controller
        .segment("1A", "main", first_sequence)
        .expect("セグメントを読めません");
    assert_mpegts(&decrypt_segment(&publisher.key(), &ivs[0], &stored));

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

#[test]
fn rejects_non_aac_audio_without_stopping_the_channel() {
    // 入力の不備を受けても channel を止めず、次の入力を待つことを確かめる
    let dir = TestDir::new("non-aac");
    let first_port = free_port();
    let second_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
"#,
            input_config("1A", first_port, true) + &input_config("1B", second_port, true)
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    let _first = Sender::start(first_port, "640x360", 30, 30, "aac", 60);
    let _second = Sender::start(second_port, "640x360", 30, 30, "mp2", 60);

    let rejected = publisher.wait_for_log(
        "channel=1B rejected the input: audio codec must be aac",
        Duration::from_secs(30),
    );
    assert!(
        rejected,
        "aac 以外の音声で 1B が拒否されません: {}",
        publisher.log()
    );
    assert!(
        !publisher.log().contains("channel=1B stopped"),
        "aac 以外の音声で 1B が止まりました: {}",
        publisher.log()
    );
    // 拒否した channel は ffmpeg を起動し直し、次の入力を待つ
    let restarted =
        publisher.wait_for_log("channel=1B starting ffmpeg run=2", Duration::from_secs(10));
    assert!(
        restarted,
        "1B の待ち受けが再開しません: {}",
        publisher.log()
    );
    assert!(
        controller.sequences("1B", "main").is_empty(),
        "aac 以外の音声のセグメントが保存されています"
    );

    // 1A は止まらずに配信を続ける
    let before = controller.sequences("1A", "main").len();
    let kept = wait_for(
        || controller.sequences("1A", "main").len() > before,
        Duration::from_secs(20),
    );
    assert!(kept, "1A の配信が止まりました: {}", publisher.log());

    // 入力の不備で publisher 全体は終了しない
    let status = publisher.stop();
    assert_eq!(
        status.code(),
        Some(0),
        "入力の拒否後に publisher が終了しました: {status}"
    );
}

#[test]
fn recovers_after_rejecting_an_input_that_does_not_match_the_rendition() {
    // 設定と違う解像度の入力で ffmpeg を起動し直したあと、
    // publisher を止めずに正しい解像度で配信を再開できることを確かめる
    let dir = TestDir::new("recover-resolution");
    let srt_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
"#,
            input_config("1A", srt_port, true)
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );

    // レンディションと違う解像度を送り、入力が拒否されるのを待つ
    let wrong = Sender::start(srt_port, "320x180", 30, 30, "aac", 20);
    let rejected = publisher.wait_for_log(
        "channel=1A rejected the input: copied video resolution must be 640x360",
        Duration::from_secs(30),
    );
    assert!(
        rejected,
        "解像度が合わない入力で 1A が拒否されません: {}",
        publisher.log()
    );
    assert!(
        !publisher.log().contains("channel=1A stopped"),
        "解像度が合わない入力で 1A が止まりました: {}",
        publisher.log()
    );
    drop(wrong);
    assert!(
        controller.sequences("1A", "main").is_empty(),
        "拒否した解像度のセグメントが保存されています"
    );

    // ffmpeg が起動し直したら、正しい解像度を送る
    let restarted =
        publisher.wait_for_log("channel=1A starting ffmpeg run=2", Duration::from_secs(10));
    assert!(
        restarted,
        "1A の待ち受けが再開しません: {}",
        publisher.log()
    );
    let _right = Sender::start(srt_port, "640x360", 30, 30, "aac", 120);

    let published = wait_for(
        || !controller.sequences("1A", "main").is_empty(),
        Duration::from_secs(40),
    );
    assert!(
        published,
        "正しい解像度の入力で配信が再開しません: {}",
        publisher.log()
    );
    assert!(publisher.is_running(), "publisher が終了しました");

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

#[test]
fn publishes_after_rejecting_an_rtmp_input_that_does_not_match_the_rendition() {
    // RTMP で解像度が合わない入力を拒否したあとも、正しい入力で publish できることを確かめる
    let dir = TestDir::new("recover-rtmp-resolution");
    let rtmp_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"

[[input]]
channel_id = "1B"
input_url = "rtmp://127.0.0.1:{rtmp_port}/live/1B"

[[input.rendition]]
quality = "main"
mode = "copy"
resolution = "640x360"
bandwidth = 12000000
"#
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );

    let wrong = Sender::start_rtmp(rtmp_port, "1B", "320x180", 30, 30, "aac", 20);
    let rejected = publisher.wait_for_log(
        "channel=1B rejected the input: copied video resolution must be 640x360",
        Duration::from_secs(30),
    );
    assert!(
        rejected,
        "RTMP の解像度が合わない入力で 1B が拒否されません: {}",
        publisher.log()
    );
    assert!(
        !publisher.log().contains("channel=1B stopped"),
        "RTMP の解像度が合わない入力で 1B が止まりました: {}",
        publisher.log()
    );
    drop(wrong);
    assert!(
        controller.sequences("1B", "main").is_empty(),
        "拒否した解像度のセグメントが保存されています"
    );

    let restarted =
        publisher.wait_for_log("channel=1B starting ffmpeg run=2", Duration::from_secs(10));
    assert!(
        restarted,
        "1B の RTMP の待ち受けが再開しません: {}",
        publisher.log()
    );
    let _right = Sender::start_rtmp(rtmp_port, "1B", "640x360", 30, 30, "aac", 120);

    let published = wait_for(
        || !controller.sequences("1B", "main").is_empty(),
        Duration::from_secs(40),
    );
    assert!(
        published,
        "正しい解像度の RTMP 入力で配信が再開しません: {}",
        publisher.log()
    );
    assert!(publisher.is_running(), "publisher が終了しました");

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

#[test]
fn drops_expired_segments_when_the_controller_is_unreachable() {
    let dir = TestDir::new("backlog");
    let srt_port = free_port();
    let unreachable = format!("http://127.0.0.1:{}", free_port());
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 2

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
"#,
            input_config("1A", srt_port, true)
        ),
    );

    let mut publisher = Publisher::start(&config, &unreachable, None, dir.path());
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    let _sender = Sender::start(srt_port, "640x360", 30, 30, "aac", 60);

    let dropped = publisher.wait_for_log("dropping expired segment", Duration::from_secs(30));
    assert!(
        dropped,
        "controller に届かないときに古いセグメントを捨てません: {}",
        publisher.log()
    );
    assert!(publisher.is_running(), "publisher が終了しました");

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

#[test]
fn warns_when_copied_segment_durations_do_not_match() {
    let dir = TestDir::new("copy-duration");
    let srt_port = free_port();
    let controller = Controller::start("test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
"#,
            input_config("1A", srt_port, true)
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    // 2.5 秒間隔のキーフレームでは 1 秒のセグメントに切れない
    let _sender = Sender::start(srt_port, "640x360", 30, 75, "aac", 60);

    let warned = publisher.wait_for_log(
        "warning: copied segment index=0 quality=main",
        Duration::from_secs(30),
    );
    assert!(
        warned,
        "コピーするセグメントの長さのずれを警告しません: {}",
        publisher.log()
    );
    let stored = wait_for(
        || !controller.sequences("1A", "main").is_empty(),
        Duration::from_secs(20),
    );
    assert!(
        stored,
        "警告のあとも配信を続けていません: {}",
        publisher.log()
    );
    assert!(publisher.is_running(), "publisher が終了しました");

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

#[test]
fn restarts_ffmpeg_after_the_sender_disconnects() {
    let dir = TestDir::new("restart");
    let srt_port = free_port();
    let controller = Controller::start("test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"
{}
"#,
            input_config("1A", srt_port, true)
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    // 4 秒で送信を終わらせ、ffmpeg を終了させる
    let _first = Sender::start(srt_port, "640x360", 30, 30, "aac", 4);
    let stored = wait_for(
        || !controller.sequences("1A", "main").is_empty(),
        Duration::from_secs(30),
    );
    assert!(
        stored,
        "最初の送信でセグメントが保存されません: {}",
        publisher.log()
    );

    let restarted = publisher.wait_for_log("starting ffmpeg run=2", Duration::from_secs(30));
    assert!(restarted, "ffmpeg が再起動しません: {}", publisher.log());
    let offset_line = publisher
        .log()
        .lines()
        .find(|line| line.contains("starting ffmpeg run=2"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        !offset_line.ends_with("offset_ms=0"),
        "再起動の時刻が続いていません: {offset_line}"
    );

    let before = controller.sequences("1A", "main").len();
    let _second = Sender::start(srt_port, "640x360", 30, 30, "aac", 60);
    let resumed = wait_for(
        || controller.sequences("1A", "main").len() > before,
        Duration::from_secs(30),
    );
    assert!(
        resumed,
        "送信を再開しても配信が続きません: {}",
        publisher.log()
    );

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

/// RTMP で受け取った映像も、暗号化されたセグメントとして保存されて復号できる
#[test]
fn publishes_segments_from_an_rtmp_input() {
    let dir = TestDir::new("rtmp");
    let rtmp_port = free_port();
    let controller = Controller::start("test-token");
    let _worker = Worker::start(&controller.base_url, "test-token");
    let config = write_config(
        dir.path(),
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"

[[input]]
channel_id = "1B"
input_url = "rtmp://127.0.0.1:{rtmp_port}/live/1B"

[[input.rendition]]
quality = "main"
mode = "copy"
resolution = "640x360"
bandwidth = 12000000
"#
        ),
    );

    let mut publisher = Publisher::start(
        &config,
        &controller.base_url,
        Some("test-token"),
        dir.path(),
    );
    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    assert!(
        publisher.wait_for_log("input_url=rtmp://127.0.0.1", Duration::from_secs(10)),
        "RTMP の入力 URL がログに出ません: {}",
        publisher.log()
    );
    let _sender = Sender::start_rtmp(rtmp_port, "1B", "640x360", 30, 30, "aac", 60);

    let stored = wait_for(
        || controller.sequences("1B", "main").len() >= 2,
        Duration::from_secs(30),
    );
    assert!(stored, "セグメントが保存されません: {}", publisher.log());
    // 送信中のセットの送信が終わるのを待つ
    thread::sleep(Duration::from_millis(500));

    let sequences = controller.sequences("1B", "main");
    let stored = controller
        .segment("1B", "main", sequences[0])
        .expect("セグメントを読めません");
    // 暗号鍵の URI を指定しない場合は controller を介して IV を伝えられないため、全ゼロ固定になる
    let iv_zero = "0".repeat(32);
    let plain = decrypt_segment(&publisher.key(), &iv_zero, &stored);
    // 暗号文の先頭バイトは同期バイトと同じ値になり得るため、平文との比較で確かめる
    assert_ne!(stored, plain, "保存されたファイルが暗号化されていません");
    assert_mpegts(&plain);
    let streams = probe_streams(dir.path(), &plain);
    assert!(
        streams.contains("h264") && streams.contains("640,360") && streams.contains("aac"),
        "復号したセグメントの内容が違います: {streams}"
    );

    let status = publisher.stop();
    assert_eq!(status.code(), Some(0), "publisher の終了コード: {status}");
}

/// stream credentials の登録テスト用に 1 channel の設定を書く
fn write_credentials_config(dir: &Path, rtmp_port: u16) -> PathBuf {
    write_config(
        dir,
        &format!(
            r#"
segment_duration_ms = 1000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"

[[input]]
channel_id = "1A"
input_url = "rtmp://127.0.0.1:{rtmp_port}/live/1A"

[[input.rendition]]
quality = "main"
resolution = "640x360"
bandwidth = 1400000
video_bitrate = 500000
"#
        ),
    )
}

/// stream credentials の登録先を指定して publisher を起動する
fn start_publisher_with_credentials(
    config: &Path,
    controller_url: &str,
    work_dir: &Path,
    credentials: &CredentialsServer,
) -> Publisher {
    Publisher::start_with_args(
        config,
        controller_url,
        Some("test-token"),
        work_dir,
        &[
            "--stream-credentials-url",
            credentials.url.as_str(),
            "--stream-ingest-token",
            "test-token",
        ],
    )
}

#[test]
fn does_not_register_stream_credentials_without_input() {
    let dir = TestDir::new("credentials-no-input");
    let rtmp_port = free_port();
    let controller = Controller::start("test-token");
    let credentials = CredentialsServer::start();
    let config = write_credentials_config(dir.path(), rtmp_port);
    let mut publisher =
        start_publisher_with_credentials(&config, &controller.base_url, dir.path(), &credentials);

    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    thread::sleep(Duration::from_secs(2));
    assert!(
        credentials.requests().is_empty(),
        "入力前に stream credentials が登録されました: {:?}",
        credentials.requests()
    );

    let status = publisher.stop();
    assert_eq!(
        status.code(),
        Some(0),
        "publisher の終了コード: {status}\n{}",
        publisher.log()
    );
}

#[test]
fn registers_stream_credentials_for_each_segment_set() {
    let dir = TestDir::new("credentials-per-set");
    let rtmp_port = free_port();
    let controller = Controller::start("test-token");
    let credentials = CredentialsServer::start();
    let config = write_credentials_config(dir.path(), rtmp_port);
    let mut publisher =
        start_publisher_with_credentials(&config, &controller.base_url, dir.path(), &credentials);

    assert!(
        publisher.wait_for_log("starting ffmpeg run=1", Duration::from_secs(10)),
        "ffmpeg が起動しません: {}",
        publisher.log()
    );
    thread::sleep(Duration::from_secs(1));
    assert!(
        credentials.requests().is_empty(),
        "入力前に stream credentials が登録されました: {:?}",
        credentials.requests()
    );

    let _sender = Sender::start_rtmp(rtmp_port, "1A", "640x360", 30, 30, "aac", 60);
    let registered = wait_for(
        || credentials.requests().len() >= 2,
        Duration::from_secs(30),
    );
    assert!(
        registered,
        "セグメントセットごとの登録が 2 回以上発生しません: {:?}\n{}",
        credentials.requests(),
        publisher.log()
    );

    for request in credentials.requests() {
        assert!(
            request.contains("\"channel_id\":\"1A\""),
            "channel_id がありません: {request}"
        );
        assert!(
            request.contains("\"candidates\""),
            "candidates がありません: {request}"
        );
    }

    let status = publisher.stop();
    assert_eq!(
        status.code(),
        Some(0),
        "publisher の終了コード: {status}\n{}",
        publisher.log()
    );
}
