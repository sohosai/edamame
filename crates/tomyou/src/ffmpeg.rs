//! ffmpeg の検出、機能検査、出力コマンドの組み立て

#[cfg(feature = "bundled-ffmpeg")]
use std::fs;
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

/// ffmpeg が今回の用途に必要な機能を持っているか
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegCapabilities {
    pub whip_muxer: bool,
    pub dtls: bool,
    pub libopus: bool,
    pub rtmp: bool,
    pub flv: bool,
    pub aac: bool,
}

impl FfmpegCapabilities {
    /// 足りない機能の名前
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.whip_muxer {
            missing.push("WHIP muxer");
        }
        if !self.dtls {
            missing.push("DTLS");
        }
        if !self.libopus {
            missing.push("libopus encoder");
        }
        if !self.rtmp {
            missing.push("RTMP protocol");
        }
        if !self.flv {
            missing.push("FLV demuxer");
        }
        if !self.aac {
            missing.push("AAC decoder");
        }
        missing
    }

    /// ImageFlux 向けの出力に必要な機能がそろっているか
    pub fn imageflux_ready(&self) -> bool {
        self.whip_muxer && self.dtls && self.libopus
    }

    /// Edamame 向けの出力に必要な機能がそろっているか
    pub fn edamame_ready(&self) -> bool {
        self.rtmp && self.flv
    }

    /// 人が読める短い説明
    pub fn summary(&self) -> String {
        let missing = self.missing();
        if missing.is_empty() {
            "WHIP / DTLS / libopus / RTMP / FLV / AAC 対応".to_owned()
        } else {
            format!("不足: {}", missing.join(", "))
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

/// 1 つの ffmpeg 出力の起動内容
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
    pub kind: OutputKind,
    pub args: Vec<String>,
}

/// ffmpeg の実行ファイルを決める。
///
/// Windows の埋め込みビルドでは、実行ファイル内の ffmpeg をユーザー領域へ展開する。
/// Linux と macOS では指定されたパス、環境変数、PATH、標準的なインストール先の順に探す。
pub fn resolve(path_hint: Option<&Path>) -> Result<PathBuf> {
    #[cfg(feature = "bundled-ffmpeg")]
    {
        return extract_embedded().context("埋め込み ffmpeg を展開できません");
    }

    #[cfg(not(feature = "bundled-ffmpeg"))]
    {
        detect_external(path_hint)
    }
}

/// PATH や標準的な場所から ffmpeg を探す
pub fn detect_external(path_hint: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path_hint
        && path.is_file()
    {
        return Ok(path.to_path_buf());
    }
    if let Some(value) = env::var_os("TOMYOU_FFMPEG") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        warn!("TOMYOU_FFMPEG is set but is not a file: {}", path.display());
    }
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(binary_name());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path) = find_in_path(binary_name()) {
        return Ok(path);
    }
    for candidate in standard_paths() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("ffmpeg が見つかりません。設定でパスを指定するか、PATH に ffmpeg を追加してください")
}

/// ffmpeg の機能を実際に起動して調べる
pub fn inspect(path: &Path) -> Result<FfmpegCapabilities> {
    let capabilities = FfmpegCapabilities {
        whip_muxer: supports(path, &["-h", "muxer=whip"], "whip")?,
        dtls: supports(path, &["-protocols"], "dtls")?,
        libopus: supports(path, &["-encoders"], "libopus")?,
        rtmp: supports(path, &["-protocols"], "rtmp")?,
        flv: supports(path, &["-demuxers"], "flv")?,
        aac: supports(path, &["-decoders"], " aac ")?,
    };
    info!(
        "ffmpeg capability check: {} ({})",
        path.display(),
        capabilities.summary()
    );
    Ok(capabilities)
}

/// Edamame publisher 向けの ffmpeg 引数
pub fn edamame_args(input_url: &str, publisher_url: &str) -> Vec<String> {
    vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-i".to_owned(),
        input_url.to_owned(),
        "-map".to_owned(),
        "0:v:0".to_owned(),
        "-map".to_owned(),
        "0:a:0?".to_owned(),
        "-c".to_owned(),
        "copy".to_owned(),
        "-f".to_owned(),
        "flv".to_owned(),
        publisher_url.to_owned(),
    ]
}

/// ImageFlux 向けの ffmpeg 引数。映像はコピーし、音声だけ Opus に変換する。
pub fn imageflux_args(input_url: &str, whip_url: &str) -> Vec<String> {
    vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-i".to_owned(),
        input_url.to_owned(),
        "-map".to_owned(),
        "0:v:0".to_owned(),
        "-map".to_owned(),
        "0:a:0?".to_owned(),
        "-c:v".to_owned(),
        "copy".to_owned(),
        "-c:a".to_owned(),
        "libopus".to_owned(),
        "-ar".to_owned(),
        "48000".to_owned(),
        "-ac".to_owned(),
        "2".to_owned(),
        "-f".to_owned(),
        "whip".to_owned(),
        whip_url.to_owned(),
    ]
}

/// 入力 URL を作る
pub fn rtmp_input(bind: &str, channel_id: &str) -> String {
    format!("rtmp://{bind}/live/{channel_id}")
}

fn supports(path: &Path, args: &[&str], needle: &str) -> Result<bool> {
    let output = Command::new(path)
        .args(args)
        .output()
        .with_context(|| format!("ffmpeg を実行できません: {}", path.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if text.contains("Unknown muxer")
        || text.contains("Unknown encoder")
        || text.contains("Unknown decoder")
        || text.contains("Unknown demuxer")
    {
        return Ok(false);
    }
    if args.first() == Some(&"-h") {
        return Ok(output.status.success() && text.contains(needle));
    }
    Ok(text
        .lines()
        .any(|line| line.split_whitespace().any(|word| word == needle.trim())))
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn standard_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        paths.push(PathBuf::from("/opt/homebrew/bin/ffmpeg"));
        paths.push(PathBuf::from("/usr/local/bin/ffmpeg"));
    }
    if cfg!(target_os = "linux") {
        paths.push(PathBuf::from("/usr/bin/ffmpeg"));
        paths.push(PathBuf::from("/usr/local/bin/ffmpeg"));
        paths.push(PathBuf::from("/snap/bin/ffmpeg"));
    }
    if cfg!(windows)
        && let Some(program_files) = env::var_os("ProgramFiles")
    {
        paths.push(PathBuf::from(program_files).join("ffmpeg/bin/ffmpeg.exe"));
    }
    paths
}

/// 埋め込み ffmpeg をユーザー領域へ展開する。
#[cfg(feature = "bundled-ffmpeg")]
fn extract_embedded() -> Result<PathBuf> {
    use std::io::Write;

    let bytes: &[u8] = include_bytes!(env!("TOMYOU_FFMPEG_PATH"));
    let digest = fnv1a(bytes);
    let dirs = directories::ProjectDirs::from("com", "sohosai", "tomyou")
        .context("ユーザーディレクトリを決められません")?;
    let directory = dirs
        .data_local_dir()
        .join("runtime")
        .join(format!("{digest:016x}"));
    fs::create_dir_all(&directory)
        .with_context(|| format!("runtime ディレクトリを作れません: {}", directory.display()))?;
    let path = directory.join(binary_name());
    if path.is_file() {
        let existing = fs::read(&path).context("既存の ffmpeg を読めません")?;
        if fnv1a(&existing) == digest {
            return Ok(path);
        }
        warn!(
            "embedded ffmpeg is corrupted, rewriting: {}",
            path.display()
        );
    }
    let temporary = directory.join(format!("{}.tmp", binary_name()));
    {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("一時ファイルを作れません: {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("ffmpeg を書けません: {}", temporary.display()))?;
        file.sync_all()
            .context("ffmpeg の書き込みを確定できません")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
            .context("ffmpeg に実行権限を付けられません")?;
    }
    fs::rename(&temporary, &path).context("ffmpeg を確定できません")?;
    Ok(path)
}

#[cfg(feature = "bundled-ffmpeg")]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edamame_args_copy_video_and_audio() {
        let args = edamame_args(
            "rtmp://127.0.0.1:1935/live/uni",
            "rtmp://publisher/live/uni",
        );
        assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(args.ends_with(&["flv".to_owned(), "rtmp://publisher/live/uni".to_owned()]));
    }

    #[test]
    fn imageflux_args_keep_video_and_transcode_audio() {
        let args = imageflux_args(
            "rtmp://127.0.0.1:1935/live/uni",
            "https://sora.example/whip/x",
        );
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-c:a", "libopus"]));
        assert!(args.windows(2).any(|pair| pair == ["-ar", "48000"]));
        assert!(args.windows(2).any(|pair| pair == ["-ac", "2"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "whip"]));
    }

    #[test]
    fn missing_features_are_reported() {
        let capabilities = FfmpegCapabilities {
            whip_muxer: false,
            dtls: true,
            libopus: false,
            rtmp: true,
            flv: true,
            aac: true,
        };
        assert_eq!(
            capabilities.missing(),
            vec!["WHIP muxer", "libopus encoder"]
        );
        assert!(!capabilities.imageflux_ready());
        assert!(capabilities.edamame_ready());
    }

    #[test]
    fn inspects_installed_ffmpeg_if_present() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg がないため、機能検査を省略します");
            return;
        }
        let capabilities = inspect(Path::new("ffmpeg")).expect("ffmpeg の機能を検査できる");
        eprintln!("ffmpeg の検査結果: {}", capabilities.summary());
        assert!(
            capabilities.rtmp,
            "RTMP が不足している: {}",
            capabilities.summary()
        );
        assert!(
            capabilities.flv,
            "FLV が不足している: {}",
            capabilities.summary()
        );
    }

    #[test]
    fn input_url_uses_live_app() {
        assert_eq!(
            rtmp_input("127.0.0.1:1935", "1A"),
            "rtmp://127.0.0.1:1935/live/1A"
        );
    }
}
