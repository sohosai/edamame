//! tomyou の設定ファイルと秘密情報の扱い

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// 配信できるチャンネル
pub const CHANNELS: [&str; 3] = ["uni", "1A", "kaikan"];

/// tomyou 全体の設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// live2025-server が認識する固定のチャンネル名
    pub channel_id: String,
    #[serde(default)]
    pub obs: ObsConfig,
    #[serde(default)]
    pub publisher: PublisherConfig,
    #[serde(default)]
    pub imageflux: ImageFluxConfig,
    #[serde(default)]
    pub relay: RelayConfig,
    #[serde(default)]
    pub ffmpeg: FfmpegConfig,
}

/// OBS WebSocket の接続設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObsConfig {
    #[serde(default = "default_obs_host")]
    pub host: String,
    #[serde(default = "default_obs_port")]
    pub port: u16,
    /// 秘密情報のため設定ファイルには保存しない
    #[serde(default, skip_serializing)]
    pub password: String,
}

/// edamame-publisher の RTMP 入力
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublisherConfig {
    /// 例: rtmp://publisher.example.test:1935/live/uni
    #[serde(default)]
    pub rtmp_url: String,
}

/// ImageFlux の API と HLS 出力設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageFluxConfig {
    /// 秘密情報のため設定ファイルには保存しない
    #[serde(default, skip_serializing)]
    pub token: String,
    #[serde(
        default,
        rename = "encrypt-key-uri",
        alias = "encrypt_key_uri",
        alias = "encryptKeyUri"
    )]
    pub encrypt_key_uri: String,
    #[serde(
        default,
        rename = "event_webhook_url",
        alias = "event-webhook-url",
        alias = "eventWebhookUrl"
    )]
    pub event_webhook_url: String,
    #[serde(default = "default_hls")]
    pub hls: Vec<HlsRendition>,
}

/// ImageFlux に作成する HLS の画質設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsRendition {
    #[serde(rename = "durationSeconds", alias = "duration-seconds")]
    pub duration_seconds: u32,
    #[serde(rename = "startTimeOffset", alias = "start-time-offset")]
    pub start_time_offset: i32,
    pub video: HlsVideo,
    pub audio: HlsAudio,
    #[serde(default)]
    pub archive: HlsArchive,
}

/// HLS の映像設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsVideo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bps: u64,
}

/// HLS の音声設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsAudio {
    pub bps: u64,
}

/// HLS のアーカイブ設定
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HlsArchive {
    #[serde(
        default,
        rename = "archive_destination_id",
        alias = "archive-destination-id",
        alias = "archiveDestinationId"
    )]
    pub archive_destination_id: String,
}

/// RTMP 受信サーバの設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    #[serde(default = "default_relay_bind")]
    pub bind: String,
}

/// ffmpeg の探索設定
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FfmpegConfig {
    /// 指定した場合は PATH より優先する
    #[serde(default)]
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channel_id: "uni".to_owned(),
            obs: ObsConfig::default(),
            publisher: PublisherConfig::default(),
            imageflux: ImageFluxConfig::default(),
            relay: RelayConfig::default(),
            ffmpeg: FfmpegConfig::default(),
        }
    }
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            host: default_obs_host(),
            port: default_obs_port(),
            password: String::new(),
        }
    }
}

impl Default for ImageFluxConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            encrypt_key_uri: String::new(),
            event_webhook_url: String::new(),
            hls: default_hls(),
        }
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: default_relay_bind(),
        }
    }
}

fn default_obs_host() -> String {
    "127.0.0.1".to_owned()
}

fn default_obs_port() -> u16 {
    4455
}

fn default_relay_bind() -> String {
    "127.0.0.1:1935".to_owned()
}

/// whip-script の example.toml と同じ初期値を返す。
fn default_hls() -> Vec<HlsRendition> {
    vec![
        HlsRendition {
            duration_seconds: 1,
            start_time_offset: -2,
            video: HlsVideo {
                width: 1920,
                height: 1080,
                fps: 60,
                bps: 15_000_000,
            },
            audio: HlsAudio { bps: 320_000 },
            archive: HlsArchive::default(),
        },
        HlsRendition {
            duration_seconds: 1,
            start_time_offset: -2,
            video: HlsVideo {
                width: 1280,
                height: 720,
                fps: 60,
                bps: 2_500_000,
            },
            audio: HlsAudio { bps: 128_000 },
            archive: HlsArchive::default(),
        },
        HlsRendition {
            duration_seconds: 1,
            start_time_offset: -2,
            video: HlsVideo {
                width: 854,
                height: 480,
                fps: 24,
                bps: 950_000,
            },
            audio: HlsAudio { bps: 96_000 },
            archive: HlsArchive::default(),
        },
    ]
}

impl Config {
    /// 設定ディレクトリの config.toml
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("com", "sohosai", "tomyou")
            .context("設定ディレクトリを決められません")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// 設定ファイルを読み、秘密情報を資格情報ストアから補う
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("設定ファイルを読めません: {}", path.display()))?;
        let mut config: Self = toml::from_str(&text)
            .with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))?;
        config.apply_secrets();
        config.validate()?;
        Ok(config)
    }

    /// 秘密情報を除いて保存し、秘密情報は資格情報ストアへ保存する
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("設定ディレクトリを作れません: {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("設定を TOML にできません")?;
        fs::write(path, text)
            .with_context(|| format!("設定ファイルを書けません: {}", path.display()))?;
        self.store_secrets()
            .context("設定ファイルは保存しましたが、秘密情報を資格情報ストアへ保存できません")?;
        Ok(())
    }

    /// 起動時に使える設定か確認する
    pub fn validate(&self) -> Result<()> {
        if !CHANNELS.contains(&self.channel_id.as_str()) {
            bail!("channel_id は uni、1A、kaikan のいずれかです");
        }
        if self.obs.host.trim().is_empty() {
            bail!("OBS のホストが空です");
        }
        if self.obs.port == 0 {
            bail!("OBS のポートが 0 です");
        }
        self.relay
            .bind
            .parse::<std::net::SocketAddr>()
            .context("relay.bind は IP:ポート の形式で指定してください")?;
        if !self.publisher.rtmp_url.trim().is_empty() {
            let url = url::Url::parse(&self.publisher.rtmp_url)
                .context("publisher.rtmp_url が URL として不正です")?;
            if url.scheme() != "rtmp" {
                bail!("publisher.rtmp_url は rtmp:// で始めてください");
            }
        }
        if let Some(path) = &self.ffmpeg.path
            && path.as_os_str().is_empty()
        {
            bail!("ffmpeg.path が空です");
        }
        if self.imageflux.hls.is_empty() {
            bail!("imageflux.hls が空です");
        }
        for rendition in &self.imageflux.hls {
            if rendition.duration_seconds == 0 {
                bail!("imageflux.hls.duration-seconds は 1 以上です");
            }
            if rendition.video.width == 0 || rendition.video.height == 0 {
                bail!("imageflux.hls.video の解像度が 0 です");
            }
            if rendition.video.fps == 0 {
                bail!("imageflux.hls.video.fps は 1 以上です");
            }
            if rendition.video.bps == 0 || rendition.audio.bps == 0 {
                bail!("imageflux.hls のビットレートは 1 以上です");
            }
        }
        Ok(())
    }

    /// 設定の秘密情報を資格情報ストアから読む。失敗しても起動は妨げない。
    fn apply_secrets(&mut self) {
        match secret::load(secret::OBS_PASSWORD) {
            Ok(Some(value)) => self.obs.password = value,
            Ok(None) => {}
            Err(error) => warn!("failed to load OBS password from keyring: {error}"),
        }
        match secret::load(secret::IMAGEFLUX_TOKEN) {
            Ok(Some(value)) => self.imageflux.token = value,
            Ok(None) => {}
            Err(error) => warn!("failed to load ImageFlux token from keyring: {error}"),
        }
    }

    /// 秘密情報を資格情報ストアへ書く。
    fn store_secrets(&self) -> Result<()> {
        secret::store(secret::OBS_PASSWORD, &self.obs.password)
            .context("OBS のパスワードを資格情報ストアへ保存できません")?;
        secret::store(secret::IMAGEFLUX_TOKEN, &self.imageflux.token)
            .context("ImageFlux のトークンを資格情報ストアへ保存できません")?;
        Ok(())
    }

    /// ImageFlux を使う設定か
    pub fn imageflux_enabled(&self) -> bool {
        !self.imageflux.token.trim().is_empty()
    }

    /// Edamame 側の送信設定がそろっているか
    pub fn edamame_enabled(&self) -> bool {
        !self.publisher.rtmp_url.trim().is_empty()
    }
}

/// OS の資格情報ストアを使う。使えない環境ではエラーを返し、呼び出し側が警告する。
mod secret {
    use anyhow::{Context, Result};

    const SERVICE: &str = "tomyou";
    pub const OBS_PASSWORD: &str = "obs-password";
    pub const IMAGEFLUX_TOKEN: &str = "imageflux-token";

    pub fn load(name: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, name).context("資格情報ストアを開けません")?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("資格情報を読めません"),
        }
    }

    pub fn store(name: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, name).context("資格情報ストアを開けません")?;
        if value.is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error).context("資格情報を削除できません"),
            }
        } else {
            entry
                .set_password(value)
                .context("資格情報を保存できません")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().expect("既定値は有効");
    }

    #[test]
    fn rejects_unknown_channel() {
        let config = Config {
            channel_id: "unknown".to_owned(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_example_config() {
        let config: Config =
            toml::from_str(include_str!("../config.example.toml")).expect("設定例を読める");
        config.validate().expect("設定例は有効");
    }

    #[test]
    fn parses_whip_script_style_toml() {
        let text = r#"
channel_id = "1A"

[publisher]
rtmp_url = "rtmp://publisher.example.test:1936/live/1A"

[imageflux]
encrypt-key-uri = "https://live.example.test/key/1A"

[[imageflux.hls]]
duration-seconds = 2
start-time-offset = -2

[imageflux.hls.video]
width = 1920
height = 1080
fps = 60
bps = 15000000

[imageflux.hls.audio]
bps = 320000

[imageflux.hls.archive]
archive_destination_id = ""
"#;
        let config: Config = toml::from_str(text).expect("TOML を読める");
        assert_eq!(config.channel_id, "1A");
        assert_eq!(config.imageflux.hls.len(), 1);
        assert_eq!(config.imageflux.hls[0].duration_seconds, 2);
        config.validate().expect("有効な設定");
    }
}
