//! GStreamer の検出、機能検査、パイプライン組み立て

use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use tracing::info;

/// GUI アプリから起動した GStreamer のコンソールアプリに、ウィンドウを開かせない
///
/// コンソールを持たないプロセスからコンソールアプリを起動すると、
/// Windows は子プロセス用に新しいコンソールウィンドウを作る。
/// 機能検査とパイプラインの起動のたびに窓が開くため、作成を抑止する。
pub(crate) fn hide_console_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        // CREATE_NO_WINDOW
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// AAC を Opus に変換する前に使うデコーダ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioDecoder {
    AvdecAac,
    Faad,
}

impl AudioDecoder {
    pub fn element(self) -> &'static str {
        match self {
            Self::AvdecAac => "avdec_aac",
            Self::Faad => "faad",
        }
    }
}

/// Edamame と ImageFlux に必要な GStreamer の機能
#[derive(Debug, Clone)]
pub struct GstCapabilities {
    available: BTreeSet<String>,
    audio_decoder: Option<AudioDecoder>,
    whip_pads_ok: bool,
}

impl GstCapabilities {
    /// Edamame 向け RTMP 出力に必要な要素
    pub fn edamame_missing(&self) -> Vec<&'static str> {
        [
            "rtmpsrc",
            "flvdemux",
            "h264parse",
            "aacparse",
            "flvmux",
            "rtmpsink",
            "queue",
        ]
        .into_iter()
        .filter(|element| !self.available.contains(*element))
        .collect()
    }

    /// ImageFlux 向け WHIP 出力に必要な要素
    pub fn imageflux_missing(&self) -> Vec<String> {
        let mut missing: Vec<String> = [
            "rtmpsrc",
            "flvdemux",
            "h264parse",
            "aacparse",
            "whipclientsink",
            "webrtcbin",
            "nicesrc",
            "nicesink",
            "opusenc",
            "audioconvert",
            "audioresample",
            "queue",
        ]
        .into_iter()
        .filter(|element| !self.available.contains(*element))
        .map(str::to_owned)
        .collect();
        if self.audio_decoder.is_none() {
            missing.push("avdec_aac または faad".to_owned());
        }
        if self.available.contains("whipclientsink") && !self.whip_pads_ok {
            missing.push("whipclientsink の audio_%u / video_%u パッド".to_owned());
        }
        missing
    }

    pub fn edamame_ready(&self) -> bool {
        self.edamame_missing().is_empty()
    }

    pub fn imageflux_ready(&self) -> bool {
        self.imageflux_missing().is_empty()
    }

    pub fn audio_decoder(&self) -> Option<AudioDecoder> {
        self.audio_decoder
    }

    pub fn summary(&self) -> String {
        let edamame = self.edamame_missing();
        let imageflux = self.imageflux_missing();
        if edamame.is_empty() && imageflux.is_empty() {
            "Edamame RTMP / ImageFlux WHIP 対応".to_owned()
        } else {
            let mut parts = Vec::new();
            if !edamame.is_empty() {
                parts.push(format!("Edamame 不足: {}", edamame.join(", ")));
            }
            if !imageflux.is_empty() {
                parts.push(format!("ImageFlux 不足: {}", imageflux.join(", ")));
            }
            parts.join(" / ")
        }
    }
}

/// 出力の種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Edamame,
    ImageFlux,
}

impl OutputKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Edamame => "Edamame",
            Self::ImageFlux => "ImageFlux",
        }
    }
}

/// 1 つの GStreamer 出力の起動内容
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
    pub kind: OutputKind,
    pub args: Vec<String>,
}

impl OutputSpec {
    fn new(kind: OutputKind, pipeline: Vec<String>) -> Self {
        // gst-launch は引数内の空白を引用扱いにするため、処理列全体を 1 引数にできない。
        let mut args = vec!["-e".to_owned()];
        args.extend(pipeline);
        Self { kind, args }
    }
}

/// GStreamer の実行ファイルを決める
pub fn resolve(path_hint: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path_hint
        && path.is_file()
    {
        return Ok(path.to_path_buf());
    }
    if let Some(value) = env::var_os("TOMYOU_GST_LAUNCH") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        bail!("TOMYOU_GST_LAUNCH is not a file: {}", path.display());
    }
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(launch_name());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path) = find_in_path(launch_name()) {
        return Ok(path);
    }
    for candidate in standard_paths() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "gst-launch-1.0 が見つかりません。設定の [gstreamer] path を指定するか、PATH に追加してください"
    )
}

/// GStreamer の要素を検査する
pub fn inspect(gst_launch: &Path) -> Result<GstCapabilities> {
    let inspect = find_inspect(gst_launch)
        .with_context(|| format!("gst-inspect-1.0 が見つかりません: {}", gst_launch.display()))?;
    let elements = [
        "rtmpsrc",
        "flvdemux",
        "h264parse",
        "aacparse",
        "flvmux",
        "rtmpsink",
        "whipclientsink",
        "webrtcbin",
        "nicesrc",
        "nicesink",
        "opusenc",
        "audioconvert",
        "audioresample",
        "queue",
    ];
    let mut available = BTreeSet::new();
    for element in elements {
        if has_element(&inspect, element) {
            available.insert(element.to_owned());
        }
    }
    let has_avdec = has_element(&inspect, "avdec_aac");
    let has_faad = has_element(&inspect, "faad");
    let audio_decoder = choose_audio_decoder(has_avdec, has_faad);
    let whip_pads_ok = has_element(&inspect, "whipclientsink") && whip_pads_ok(&inspect);
    let capabilities = GstCapabilities {
        available,
        audio_decoder,
        whip_pads_ok,
    };
    info!(
        "GStreamer capability check: {} (edamame_ready={}, imageflux_ready={})",
        gst_launch.display(),
        capabilities.edamame_ready(),
        capabilities.imageflux_ready()
    );
    Ok(capabilities)
}

/// Edamame publisher 向けの RTMP パイプライン
pub fn edamame_pipeline(input: &str, publisher: &str) -> Vec<String> {
    [
        "rtmpsrc",
        &rtmp_location(input),
        "!",
        "flvdemux",
        "name=demux",
        "demux.video",
        "!",
        "h264parse",
        "!",
        "queue",
        "!",
        "mux.",
        "demux.audio",
        "!",
        "aacparse",
        "!",
        "queue",
        // 推奨する 2 秒間隔のキーフレームを待つ間、音声の既定 1 秒制限で映像の読み取りまで止まる。
        "max-size-time=5000000000",
        "max-size-buffers=0",
        "max-size-bytes=0",
        "!",
        "mux.",
        "flvmux",
        "name=mux",
        "streamable=true",
        "!",
        "rtmpsink",
        &format!("location={publisher}"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// ImageFlux 向けの WHIP パイプライン。
///
/// 音声を先に接続して m-line を audio0, video1 の順にする。
/// Sora の answer は音声を先頭に返すため、この順序が必要である。
pub fn imageflux_pipeline(input: &str, endpoint: &str, decoder: AudioDecoder) -> Vec<String> {
    [
        "rtmpsrc",
        &rtmp_location(input),
        "!",
        "flvdemux",
        "name=demux",
        "demux.audio",
        "!",
        "aacparse",
        "!",
        decoder.element(),
        "!",
        "audioconvert",
        "!",
        "audioresample",
        "!",
        "audio/x-raw,rate=48000,channels=2",
        "!",
        "opusenc",
        "bitrate=128000",
        "!",
        "queue",
        // 映像の最初のキーフレームが届くまで、音声の背圧で共通の RTMP 入力を止めない。
        "max-size-time=5000000000",
        "max-size-buffers=0",
        "max-size-bytes=0",
        "!",
        "whip.audio_0",
        "demux.video",
        "!",
        "h264parse",
        "config-interval=-1",
        "!",
        "queue",
        "!",
        "whip.video_0",
        "whipclientsink",
        "name=whip",
        &format!("signaller::whip-endpoint={endpoint}"),
        "signaller::use-link-headers=true",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn rtmp_location(input: &str) -> String {
    // librtmp の接続オプションは独立した引数ではなく、location の値に含める必要がある。
    format!("location={input} live=1 flashver=tomyou")
}

/// Edamame 向けの出力定義
pub fn edamame_spec(input: &str, publisher: &str) -> OutputSpec {
    OutputSpec::new(OutputKind::Edamame, edamame_pipeline(input, publisher))
}

/// ImageFlux 向けの出力定義
pub fn imageflux_spec(input: &str, endpoint: &str, decoder: AudioDecoder) -> OutputSpec {
    OutputSpec::new(
        OutputKind::ImageFlux,
        imageflux_pipeline(input, endpoint, decoder),
    )
}

/// RTMP 入力 URL を作る
pub fn rtmp_input(bind: &str, channel_id: &str) -> String {
    format!("rtmp://{bind}/live/{channel_id}")
}

fn choose_audio_decoder(has_avdec: bool, has_faad: bool) -> Option<AudioDecoder> {
    if has_avdec {
        Some(AudioDecoder::AvdecAac)
    } else if has_faad {
        Some(AudioDecoder::Faad)
    } else {
        None
    }
}

fn has_element(inspect: &Path, element: &str) -> bool {
    let mut command = Command::new(inspect);
    hide_console_window(&mut command);
    command
        .arg(element)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn whip_pads_ok(inspect: &Path) -> bool {
    let mut command = Command::new(inspect);
    hide_console_window(&mut command);
    let Ok(output) = command.arg("whipclientsink").output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("audio_%u") && text.contains("video_%u")
}

fn find_inspect(gst_launch: &Path) -> Option<PathBuf> {
    if let Some(parent) = gst_launch.parent() {
        let candidate = parent.join(inspect_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_in_path(inspect_name())
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn launch_name() -> &'static str {
    if cfg!(windows) {
        "gst-launch-1.0.exe"
    } else {
        "gst-launch-1.0"
    }
}

fn inspect_name() -> &'static str {
    if cfg!(windows) {
        "gst-inspect-1.0.exe"
    } else {
        "gst-inspect-1.0"
    }
}

fn standard_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        paths.push(PathBuf::from("/opt/homebrew/bin/gst-launch-1.0"));
        paths.push(PathBuf::from("/usr/local/bin/gst-launch-1.0"));
        paths.push(PathBuf::from(
            "/Library/Frameworks/GStreamer.framework/Versions/1.0/bin/gst-launch-1.0",
        ));
    }
    if cfg!(target_os = "linux") {
        paths.push(PathBuf::from("/usr/bin/gst-launch-1.0"));
        paths.push(PathBuf::from("/usr/local/bin/gst-launch-1.0"));
    }
    if cfg!(windows) {
        for root in [
            "C:\\gstreamer\\1.0\\msvc_x86_64",
            "C:\\gstreamer\\1.0\\mingw_x86_64",
        ] {
            paths.push(PathBuf::from(root).join("bin/gst-launch-1.0.exe"));
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(elements: &[&str], decoder: Option<AudioDecoder>) -> GstCapabilities {
        GstCapabilities {
            available: elements.iter().map(|value| (*value).to_owned()).collect(),
            audio_decoder: decoder,
            whip_pads_ok: true,
        }
    }

    #[test]
    fn edamame_pipeline_uses_flvmux_and_rtmpsink() {
        let pipeline =
            edamame_pipeline("rtmp://127.0.0.1:1935/live/1A", "rtmp://publisher/live/1A").join(" ");
        assert!(pipeline.contains("rtmpsrc"));
        assert!(pipeline.contains("flvmux"));
        assert!(pipeline.contains("rtmpsink"));
        assert!(!pipeline.contains("whipclientsink"));
    }

    #[test]
    fn imageflux_pipeline_connects_audio_before_video() {
        let pipeline = imageflux_pipeline(
            "rtmp://127.0.0.1:1935/live/1A",
            "https://sora.example/whip/abc",
            AudioDecoder::AvdecAac,
        )
        .join(" ");
        assert!(pipeline.contains("whipclientsink"));
        assert!(pipeline.contains("signaller::whip-endpoint"));
        assert!(pipeline.contains("signaller::use-link-headers=true"));
        assert!(pipeline.find("whip.audio_0").unwrap() < pipeline.find("whip.video_0").unwrap());
    }

    #[test]
    fn chooses_avdec_before_faad() {
        assert_eq!(
            choose_audio_decoder(true, true),
            Some(AudioDecoder::AvdecAac)
        );
        assert_eq!(choose_audio_decoder(false, true), Some(AudioDecoder::Faad));
        assert_eq!(choose_audio_decoder(false, false), None);
    }

    #[test]
    fn launch_arguments_run_with_installed_gstreamer() {
        // 文字列比較では検出できない引数の引用処理を、実際のパーサと有限の処理列で検証する。
        let Ok(launch) = resolve(None) else {
            eprintln!("GStreamer がないため、処理列の実行検証を省略します");
            return;
        };
        let spec = OutputSpec::new(
            OutputKind::Edamame,
            ["fakesrc", "num-buffers=1", "!", "fakesink"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        let output = Command::new(launch)
            .args(spec.args)
            .output()
            .expect("GStreamer を実行できる");
        assert!(
            output.status.success(),
            "処理列の実行に失敗しました: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn output_urls_remain_single_property_arguments() {
        // URL の空白や記号を処理列の区切りと誤認しないよう、プロパティ単位で保持する。
        let input = "rtmp://127.0.0.1/live/key token=value";
        let publisher = "rtmp://127.0.0.1/live/output token=value";
        let endpoint = "https://example.test/whip?token=a&value=b";
        let edamame = edamame_spec(input, publisher);
        let imageflux = imageflux_spec(input, endpoint, AudioDecoder::AvdecAac);
        for spec in [&edamame, &imageflux] {
            assert!(spec.args.contains(&rtmp_location(input)));
            assert!(spec.args.iter().any(|arg| arg == "!"));
        }
        assert!(edamame.args.contains(&format!("location={publisher}")));
        assert!(
            imageflux
                .args
                .contains(&format!("signaller::whip-endpoint={endpoint}"))
        );
    }

    #[test]
    fn reports_missing_edamame_elements() {
        let capabilities = capabilities(&["rtmpsrc", "flvdemux"], Some(AudioDecoder::AvdecAac));
        assert!(!capabilities.edamame_ready());
        assert!(capabilities.edamame_missing().contains(&"flvmux"));
    }

    #[test]
    fn inspects_installed_gstreamer_if_present() {
        if Command::new("gst-launch-1.0")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("GStreamer がないため、機能検査を省略します");
            return;
        }
        let path = resolve(None).expect("gst-launch-1.0 を見つけられる");
        let capabilities = inspect(&path).expect("GStreamer の機能を検査できる");
        eprintln!("GStreamer の検査結果: {}", capabilities.summary());
        assert!(
            capabilities.edamame_ready(),
            "Edamame 向けの GStreamer 要素が足りません: {}",
            capabilities.summary()
        );
    }

    #[test]
    fn reports_missing_imageflux_pads_and_decoder() {
        let capabilities = GstCapabilities {
            available: [
                "rtmpsrc",
                "flvdemux",
                "h264parse",
                "aacparse",
                "whipclientsink",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            audio_decoder: None,
            whip_pads_ok: false,
        };
        let missing = capabilities.imageflux_missing();
        assert!(missing.iter().any(|value| value.contains("whipclientsink")));
        assert!(missing.iter().any(|value| value.contains("avdec_aac")));
    }

    #[test]
    fn requires_ice_elements_even_when_whip_is_installed() {
        // whipclientsink の存在だけでは、webrtcbin が接続用パッドを作れるとは限らない。
        let elements = [
            "rtmpsrc",
            "flvdemux",
            "h264parse",
            "aacparse",
            "whipclientsink",
            "webrtcbin",
            "opusenc",
            "audioconvert",
            "audioresample",
            "queue",
        ];
        let incomplete = capabilities(&elements, Some(AudioDecoder::AvdecAac));
        assert!(!incomplete.imageflux_ready());
        assert_eq!(incomplete.imageflux_missing(), vec!["nicesrc", "nicesink"]);

        let mut complete = incomplete;
        complete
            .available
            .extend(["nicesrc".to_owned(), "nicesink".to_owned()]);
        assert!(complete.imageflux_ready());
    }
}
