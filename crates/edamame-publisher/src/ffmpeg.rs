//! ffmpeg と ffprobe の起動、出力の読み取り、プレイリストの解析

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;

use crate::config::{PublisherSettings, RenditionMode, RenditionSettings};

/// publisher が HLS muxer の出力先として指定するプレイリストのファイル名
pub const PLAYLIST_NAME: &str = "index.m3u8";

/// ffmpeg に渡す引数を組み立てる
#[must_use]
pub fn build_command(
    settings: &PublisherSettings,
    input_url: &str,
    renditions: &[RenditionSettings],
    run_dir: &Path,
    offset_ms: u64,
) -> Vec<String> {
    let segment_seconds = format_seconds(settings.segment_duration_ms);
    let mut args = vec![
        "ffmpeg".to_owned(),
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-stats".to_owned(),
        "-stats_period".to_owned(),
        "60".to_owned(),
    ];
    // QSV は入力を指定する前に有効にし、デコードしたフレームをハードウェアメモリのまま扱う
    if settings.encoder.codec.ends_with("_qsv") {
        args.extend([
            "-hwaccel".to_owned(),
            "qsv".to_owned(),
            "-hwaccel_output_format".to_owned(),
            "qsv".to_owned(),
        ]);
    }
    // RTMP は publisher が接続を待ち受ける。SRT は URL の mode で待ち受ける
    if input_url.starts_with("rtmp://") {
        args.extend(["-listen".to_owned(), "1".to_owned()]);
    }
    args.extend(["-i".to_owned(), input_url.to_owned()]);

    for rendition in renditions {
        args.extend([
            "-map".to_owned(),
            "0:v:0".to_owned(),
            "-map".to_owned(),
            "0:a:0?".to_owned(),
        ]);
        match rendition.mode {
            RenditionMode::Copy => {
                args.extend(["-c:v".to_owned(), "copy".to_owned()]);
            }
            RenditionMode::Encode => {
                args.extend([
                    "-c:v".to_owned(),
                    settings.encoder.codec.clone(),
                    "-preset".to_owned(),
                    settings.encoder.preset.clone(),
                ]);
                // QSV はハードウェアフレームのまま扱うため、ピクセル形式の変換を挟まない
                if !settings.encoder.codec.ends_with("_qsv") {
                    args.extend(["-pix_fmt".to_owned(), "yuv420p".to_owned()]);
                }
                // 強制したキーフレームが IDR にならないと、セグメントが指定した長さで切れない
                if let Some(option) = forced_idr_option(&settings.encoder.codec) {
                    args.extend([option.to_owned(), "1".to_owned()]);
                }
                let bitrate = rendition.video_bitrate.unwrap_or(0);
                args.extend([
                    "-vf".to_owned(),
                    scale_filter(&settings.encoder.codec, rendition),
                    "-b:v".to_owned(),
                    bitrate.to_string(),
                    "-maxrate".to_owned(),
                    bitrate.to_string(),
                    "-bufsize".to_owned(),
                    (bitrate * 2).to_string(),
                    "-force_key_frames".to_owned(),
                    format!("expr:gte(t,n_forced*{segment_seconds})"),
                ]);
            }
        }
        args.extend(["-c:a".to_owned(), "copy".to_owned()]);

        let quality_dir = run_dir.join(&rendition.quality);
        args.extend([
            "-output_ts_offset".to_owned(),
            format_seconds(offset_ms),
            "-f".to_owned(),
            "hls".to_owned(),
            "-hls_time".to_owned(),
            segment_seconds.clone(),
            "-hls_list_size".to_owned(),
            "3".to_owned(),
            "-hls_flags".to_owned(),
            "temp_file".to_owned(),
            "-hls_segment_type".to_owned(),
            "mpegts".to_owned(),
            "-hls_segment_filename".to_owned(),
            quality_dir.join("%d.ts").to_string_lossy().into_owned(),
            quality_dir
                .join(PLAYLIST_NAME)
                .to_string_lossy()
                .into_owned(),
        ]);
    }
    args
}

/// 強制したキーフレームを IDR にするオプション。
/// 指定が要らないエンコーダでは None を返す
fn forced_idr_option(codec: &str) -> Option<&'static str> {
    if codec.ends_with("_qsv") {
        Some("-forced_idr")
    } else if codec.ends_with("nvenc") {
        Some("-forced-idr")
    } else {
        None
    }
}

/// rendition の解像度へ変換するフィルタ。
/// QSV はハードウェアフレームのまま拡大縮小できるフィルタを使う。
/// Arc B580 で 540p から 1080p に拡大して比べたとき、scale_qsv の Y-PSNR (40.5 dB) は
/// lanczos (40.0 dB) より高かったため、CPU の lanczos には替えない。
/// それ以外は既定の bicubic より輪郭の残る lanczos を使う
fn scale_filter(codec: &str, rendition: &RenditionSettings) -> String {
    let width = rendition.resolution.width;
    let height = rendition.resolution.height;
    if codec.ends_with("_qsv") {
        format!("scale_qsv=w={width}:h={height},setsar=1")
    } else {
        format!("scale={width}:{height}:flags=lanczos,setsar=1")
    }
}

/// 起動中の ffmpeg
pub struct Ffmpeg {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Ffmpeg {
    /// ffmpeg を起動し、stderr を publisher のログへ転送する
    pub fn spawn(args: &[String], channel_id: &str) -> Result<Self, String> {
        let (program, rest) = args
            .split_first()
            .ok_or_else(|| "ffmpeg command is empty".to_owned())?;
        let mut child = Command::new(program)
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to start ffmpeg: {error}"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to read ffmpeg stderr".to_owned())?;
        let prefix = format!("channel={channel_id} ffmpeg");
        let reader = thread::spawn(move || forward_lines(stderr, &prefix));
        Ok(Self {
            child,
            reader: Some(reader),
        })
    }

    /// ffmpeg が終了していれば終了状態を返す
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        self.child
            .try_wait()
            .map_err(|error| format!("failed to check ffmpeg: {error}"))
    }

    /// ffmpeg を止め、ログ転送スレッドの終了を待つ
    pub fn stop(&mut self) {
        stop_child(&mut self.child);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        self.stop();
    }
}

/// ffmpeg に終了を促し、応答がなければ強制終了する
fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    // SIGTERM で最後のセグメントを書き終えさせ、応答がなければ SIGKILL にする
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// ffmpeg の出力を行単位で転送する
///
/// 進捗の行は改行ではなく復帰で区切られるため、復帰も行の区切りとして扱う。
fn forward_lines(mut reader: impl Read, prefix: &str) {
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                for byte in &chunk[..size] {
                    if *byte == b'\n' || *byte == b'\r' {
                        print_line(prefix, &line);
                        line.clear();
                    } else {
                        line.push(*byte);
                    }
                }
            }
        }
    }
    print_line(prefix, &line);
}

/// 1 行をログに出す
fn print_line(prefix: &str, bytes: &[u8]) {
    let line = String::from_utf8_lossy(bytes).trim().to_owned();
    if !line.is_empty() {
        println!("{prefix}: {line}");
    }
}

/// ffprobe が返す 1 つのストリーム
#[derive(Debug, Clone, Deserialize)]
pub struct StreamInfo {
    #[serde(default)]
    pub codec_type: String,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub field_order: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    streams: Vec<StreamInfo>,
}

/// セグメントファイルのストリームを調べる
pub fn probe_streams(path: &Path) -> Result<Vec<StreamInfo>, String> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output()
        .map_err(|error| format!("failed to run ffprobe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed to parse ffprobe output: {error}"))?;
    Ok(parsed.streams)
}

/// プレイリストから (ファイル名, 秒単位の長さ) を順に取り出す
#[must_use]
pub fn parse_segment_durations(text: &str) -> Vec<(String, f64)> {
    let mut segments = Vec::new();
    let mut duration = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest
                .split(',')
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some(seconds) = duration.take()
        {
            segments.push((line.to_owned(), seconds));
        }
    }
    segments
}

/// ミリ秒を ffmpeg が解釈できる秒の文字列にする
#[must_use]
pub fn format_seconds(millis: u64) -> String {
    let whole = millis / 1000;
    let fraction = millis % 1000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:03}")
            .trim_end_matches('0')
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ControllerSettings, EncoderSettings, InputSettings, Resolution};

    /// テストで使う設定
    fn settings() -> PublisherSettings {
        PublisherSettings {
            segment_duration_ms: 6000,
            backlog_limit: 6,
            encoder: EncoderSettings {
                codec: "h264_nvenc".to_owned(),
                preset: "p5".to_owned(),
            },
            controllers: vec![ControllerSettings {
                url: "http://127.0.0.1:9000".to_owned(),
                metric: 1,
            }],
            auth_token: None,
            key_file: None,
            stream_credentials_url: None,
            stream_ingest_token: None,
            inputs: vec![InputSettings {
                channel_id: "1A".to_owned(),
                input_url: "srt://0.0.0.0:9101?mode=listener".to_owned(),
                live_url: None,
                key_uri: None,
                renditions: Vec::new(),
            }],
        }
    }

    /// コピーする rendition とエンコードする rendition
    fn renditions() -> Vec<RenditionSettings> {
        vec![
            RenditionSettings {
                quality: "1080p".to_owned(),
                mode: RenditionMode::Copy,
                resolution: Resolution {
                    width: 1920,
                    height: 1080,
                },
                bandwidth: 12_000_000,
                video_bitrate: None,
            },
            RenditionSettings {
                quality: "720p".to_owned(),
                mode: RenditionMode::Encode,
                resolution: Resolution {
                    width: 1280,
                    height: 720,
                },
                bandwidth: 6_500_000,
                video_bitrate: Some(6_000_000),
            },
        ]
    }

    #[test]
    fn builds_command_for_copy_and_encode() {
        let args = build_command(
            &settings(),
            "srt://0.0.0.0:9101?mode=listener",
            &renditions(),
            Path::new("/tmp/run-1"),
            6000,
        );
        let text = args.join(" ");
        assert!(text.contains("-c:v copy"));
        assert!(text.contains("-c:v h264_nvenc"));
        assert!(text.contains("-preset p5"));
        assert!(text.contains("-forced-idr 1"));
        assert!(text.contains("scale=1280:720:flags=lanczos,setsar=1"));
        assert!(text.contains("expr:gte(t,n_forced*6)"));
        assert!(text.contains("-output_ts_offset 6"));
        assert!(text.contains("/tmp/run-1/1080p/%d.ts"));
        assert!(text.contains("/tmp/run-1/720p/index.m3u8"));
        assert!(!text.contains("-video_bitrate"));
    }

    #[test]
    fn omits_forced_idr_for_other_encoders() {
        let mut target = settings();
        target.encoder.codec = "libx264".to_owned();
        let args = build_command(
            &target,
            "srt://0.0.0.0:9101?mode=listener",
            &renditions(),
            Path::new("/tmp/run-1"),
            0,
        );
        assert!(!args.join(" ").contains("-forced-idr"));
    }

    #[test]
    fn listens_for_an_rtmp_input() {
        let args = build_command(
            &settings(),
            "rtmp://0.0.0.0:1936/live/1A",
            &renditions(),
            Path::new("/tmp/run-1"),
            0,
        );
        assert!(
            args.join(" ")
                .contains("-listen 1 -i rtmp://0.0.0.0:1936/live/1A")
        );
    }

    #[test]
    fn does_not_listen_for_an_srt_input() {
        let args = build_command(
            &settings(),
            "srt://0.0.0.0:9101?mode=listener",
            &renditions(),
            Path::new("/tmp/run-1"),
            0,
        );
        assert!(!args.join(" ").contains("-listen"));
    }

    #[test]
    fn builds_command_for_qsv() {
        let mut target = settings();
        target.encoder.codec = "h264_qsv".to_owned();
        target.encoder.preset = "medium".to_owned();
        let args = build_command(
            &target,
            "rtmp://0.0.0.0:1936/live/1A",
            &renditions(),
            Path::new("/tmp/run-1"),
            0,
        );
        let text = args.join(" ");
        assert!(text.contains("-hwaccel qsv -hwaccel_output_format qsv -listen 1 -i rtmp://"));
        assert!(text.contains("-forced_idr 1"));
        assert!(text.contains("-vf scale_qsv=w=1280:h=720,setsar=1"));
        // ハードウェアフレームを渡す経路ではピクセル形式を指定しない
        assert!(!text.contains("-pix_fmt"));
        // NVEnc 用のオプションは付けない
        assert!(!text.contains("-forced-idr 1"));
        // copy の rendition は QSV でもそのままコピーする
        assert!(text.contains("-c:v copy"));
    }

    #[test]
    fn parses_playlist_durations() {
        let text =
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.006000,\n3.ts\n#EXTINF:6.006000,\n4.ts\n";
        let segments = parse_segment_durations(text);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].0, "3.ts");
        assert!((segments[1].1 - 6.006).abs() < 0.0001);
    }

    #[test]
    fn formats_seconds_without_rounding() {
        assert_eq!(format_seconds(0), "0");
        assert_eq!(format_seconds(6000), "6");
        assert_eq!(format_seconds(6500), "6.5");
        assert_eq!(format_seconds(6006), "6.006");
    }
}
