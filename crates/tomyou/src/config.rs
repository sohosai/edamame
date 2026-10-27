//! tomyou の設定ファイル

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
    pub stream_credentials: StreamCredentialsConfig,
    #[serde(default)]
    pub imageflux: ImageFluxConfig,
    #[serde(default)]
    pub relay: RelayConfig,
    #[serde(default)]
    pub gstreamer: GStreamerConfig,
}

/// OBS WebSocket の接続設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObsConfig {
    #[serde(default = "default_obs_host")]
    pub host: String,
    #[serde(default = "default_obs_port")]
    pub port: u16,
    /// 設定ファイルに保存する
    #[serde(default)]
    pub password: String,
}

/// edamame-publisher の送信先
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherConfig {
    /// publisher のホスト。ポートを省略するとチャンネルごとの既定値を使う
    ///
    /// 例: publisher.example.test
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl PublisherConfig {
    /// 前後の空白と空行を除いたホストを返す
    pub fn hosts(&self) -> Vec<&str> {
        self.hosts
            .iter()
            .map(|host| host.trim())
            .filter(|host| !host.is_empty())
            .collect()
    }

    /// publisher の送信先 URL を上から順に組み立てる
    pub fn urls(&self, channel_id: &str) -> Result<Vec<String>> {
        self.hosts()
            .into_iter()
            .map(|host| publisher_url(host, channel_id))
            .collect()
    }
}

/// チャンネルごとの publisher 側 RTMP ポート
///
/// docs/tomyou.md の表と一致させる。
fn channel_port(channel_id: &str) -> Option<u16> {
    match channel_id {
        "uni" => Some(1935),
        "1A" => Some(1936),
        "kaikan" => Some(1937),
        _ => None,
    }
}

/// ホストとチャンネルから publisher の送信先 URL を作る
fn publisher_url(host: &str, channel_id: &str) -> Result<String> {
    // URL を組み立てる前に、ホスト以外の要素が混ざっていないか確認する
    if host.contains("://")
        || host.chars().any(|character| {
            character.is_whitespace() || matches!(character, '/' | '?' | '#' | '@')
        })
    {
        bail!("publisher.hosts はホストと任意のポートだけを指定してください: {host}");
    }
    let probe = url::Url::parse(&format!("rtmp://{host}/"))
        .with_context(|| format!("publisher.hosts の値が不正です: {host}"))?;
    let host_text = probe
        .host_str()
        .with_context(|| format!("publisher.hosts にホストがありません: {host}"))?;
    let port = match probe.port() {
        Some(0) => bail!("publisher.hosts のポートは 1 以上です: {host}"),
        Some(port) => port,
        None => channel_port(channel_id).with_context(|| {
            format!(
                "channel {channel_id} の publisher ポートが決まりません。host:port の形式で指定してください: {host}"
            )
        })?,
    };
    let text = format!("rtmp://{host_text}:{port}/live/{channel_id}");
    let url = url::Url::parse(&text)
        .with_context(|| format!("publisher の送信先が URL として不正です: {text}"))?;
    // 想定外のユーザー情報やパスが混ざっていないことを確認する
    if url.scheme() != "rtmp"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != format!("/live/{channel_id}")
    {
        bail!("publisher の送信先が不正です: {text}");
    }
    Ok(text)
}

/// ImageFlux を fallback 配信として登録する live2025-server の設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamCredentialsConfig {
    /// live2025-server のベース URL
    #[serde(default)]
    pub base_url: String,
    /// live2025-server の STREAM_INGEST_TOKEN
    #[serde(default)]
    pub ingest_token: String,
    /// ImageFlux を登録するときの metric。小さいほど優先される
    #[serde(default = "default_imageflux_metric")]
    pub metric: u32,
}

fn default_imageflux_metric() -> u32 {
    100_000
}

impl Default for StreamCredentialsConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            ingest_token: String::new(),
            metric: default_imageflux_metric(),
        }
    }
}

impl StreamCredentialsConfig {
    /// fallback 登録を使う設定か
    pub fn enabled(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.ingest_token.trim().is_empty()
    }

    /// ベース URL を検証し、末尾の / を除いた文字列を返す
    fn normalized_base_url(&self) -> Result<&str> {
        let base_url = self.base_url.trim();
        let url = url::Url::parse(base_url)
            .context("stream_credentials.base_url が URL として不正です")?;
        if url.scheme() != "https" {
            bail!("stream_credentials.base_url は https:// で始めてください");
        }
        if url.host_str().is_none() {
            bail!("stream_credentials.base_url にホストがありません");
        }
        if url.path() != "/" && !url.path().is_empty() {
            bail!("stream_credentials.base_url にパスは指定できません");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("stream_credentials.base_url にクエリやフラグメントは指定できません");
        }
        Ok(base_url.trim_end_matches('/'))
    }

    /// 設定の矛盾を確認する
    fn validate(&self) -> Result<()> {
        if self.metric > i32::MAX as u32 {
            bail!("stream_credentials.metric は 32bit 符号付き整数に収めてください");
        }
        let base_url = self.base_url.trim();
        let ingest_token = self.ingest_token.trim();
        if base_url.is_empty() && ingest_token.is_empty() {
            return Ok(());
        }
        if base_url.is_empty() || ingest_token.is_empty() {
            bail!("stream_credentials.base_url と ingest_token は両方指定してください");
        }
        self.normalized_base_url()?;
        if !ingest_token
            .chars()
            .all(|character| ('!'..='~').contains(&character))
        {
            bail!(
                "stream_credentials.ingest_token は空白を含まない印字可能 ASCII で指定してください"
            );
        }
        Ok(())
    }

    /// ImageFlux に渡す暗号鍵 URI を組み立てる
    pub fn key_uri(&self, channel_id: &str) -> Result<String> {
        if !CHANNELS.contains(&channel_id) {
            bail!("channel_id は uni、1A、kaikan のいずれかです");
        }
        let base_url = self.normalized_base_url()?;
        Ok(format!(
            "{base_url}/key/{channel_id}?metric={}",
            self.metric
        ))
    }
}

/// ImageFlux の API と HLS 出力設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageFluxConfig {
    /// 設定ファイルに保存する
    #[serde(default)]
    pub token: String,
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

/// GStreamer の探索設定
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GStreamerConfig {
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
            stream_credentials: StreamCredentialsConfig::default(),
            imageflux: ImageFluxConfig::default(),
            relay: RelayConfig::default(),
            gstreamer: GStreamerConfig::default(),
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

    /// 設定ファイルを読む
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("設定ファイルを読めません: {}", path.display()))?;
        let mut config: Self = toml::from_str(&text)
            .with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))?;
        config.normalize_channel_id();
        config.validate()?;
        Ok(config)
    }

    /// パスワードと API トークンも含めて設定ファイルへ保存する
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("設定ディレクトリを作れません: {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("設定を TOML にできません")?;
        fs::write(path, text)
            .with_context(|| format!("設定ファイルを書けません: {}", path.display()))?;
        // 秘密情報を含むため、Unix では所有者だけが読める権限にする
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("設定ファイルの権限を変えられません: {}", path.display())
            })?;
        }
        Ok(())
    }

    /// 設定ファイルのチャンネル名を、サーバーが使う大文字小文字へそろえる
    fn normalize_channel_id(&mut self) {
        if let Some(channel) = CHANNELS
            .iter()
            .find(|channel| channel.eq_ignore_ascii_case(&self.channel_id))
        {
            self.channel_id = (*channel).to_owned();
        }
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
        let relay_bind: std::net::SocketAddr = self
            .relay
            .bind
            .parse()
            .context("relay.bind は IP:ポート の形式で指定してください")?;
        // ポート 0 だと画面に出す URL と実際の待ち受けポートがずれる
        if relay_bind.port() == 0 {
            bail!("relay.bind のポートは 1 以上です");
        }
        // 全ホストについて、URL を組み立てられることを確認する
        self.publisher.urls(&self.channel_id)?;
        if let Some(path) = &self.gstreamer.path
            && path.as_os_str().is_empty()
        {
            bail!("gstreamer.path が空です");
        }
        self.stream_credentials.validate()?;
        if self.stream_credentials.enabled() {
            if !self.imageflux_enabled() {
                bail!("fallback 登録には imageflux.token が必要です");
            }
            let _ = self.imageflux_key_uri()?;
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

    /// ImageFlux を使う設定か
    pub fn imageflux_enabled(&self) -> bool {
        !self.imageflux.token.trim().is_empty()
    }

    /// ImageFlux に渡す暗号鍵 URI を返す
    pub fn imageflux_key_uri(&self) -> Result<String> {
        if !self.stream_credentials.enabled() {
            return Ok(String::new());
        }
        self.stream_credentials.key_uri(&self.channel_id)
    }

    /// Edamame 側の送信設定がそろっているか
    pub fn edamame_enabled(&self) -> bool {
        !self.publisher.hosts().is_empty()
    }

    /// OBS に設定する RTMP サーバの URL
    pub fn relay_server(&self) -> String {
        format!("rtmp://{}/live", self.relay.bind)
    }

    /// OBS から tomyou へ接続するときの RTMP URL
    pub fn relay_url(&self) -> String {
        format!("{}/{}", self.relay_server(), self.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fallback 登録を有効にした設定を作る
    fn fallback_config() -> Config {
        Config {
            channel_id: "1A".to_owned(),
            stream_credentials: StreamCredentialsConfig {
                base_url: "https://live.example.test".to_owned(),
                ingest_token: "ingest-token".to_owned(),
                ..StreamCredentialsConfig::default()
            },
            imageflux: ImageFluxConfig {
                token: "imageflux-secret".to_owned(),
                ..ImageFluxConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().expect("既定値は有効");
    }

    #[test]
    fn publisher_hosts_build_urls_with_channel_ports() {
        let mut config = Config::default();
        config.publisher.hosts = vec![
            "first.example.test".to_owned(),
            "  second.example.test:4001  ".to_owned(),
            "   ".to_owned(),
        ];
        assert_eq!(
            config.publisher.urls("1A").expect("送信先を組み立てられる"),
            vec![
                "rtmp://first.example.test:1936/live/1A",
                "rtmp://second.example.test:4001/live/1A"
            ]
        );
        assert!(config.edamame_enabled());
        config.validate().expect("複数の送信先も有効");
    }

    #[test]
    fn publisher_hosts_use_default_port_per_channel() {
        for (channel, port) in [("uni", 1935), ("1A", 1936), ("kaikan", 1937)] {
            let config = Config {
                channel_id: channel.to_owned(),
                publisher: PublisherConfig {
                    hosts: vec!["publisher.example.test".to_owned()],
                },
                ..Config::default()
            };
            assert_eq!(
                config
                    .publisher
                    .urls(channel)
                    .expect("送信先を組み立てられる"),
                vec![format!(
                    "rtmp://publisher.example.test:{port}/live/{channel}"
                )]
            );
            config.validate().expect("既定ポートで有効");
        }
    }

    #[test]
    fn publisher_host_supports_ipv6() {
        let mut config = Config::default();
        config.publisher.hosts = vec!["[::1]".to_owned()];
        assert_eq!(
            config.publisher.urls("1A").expect("送信先を組み立てられる"),
            vec!["rtmp://[::1]:1936/live/1A"]
        );
    }

    #[test]
    fn rejects_publisher_host_with_scheme_or_path() {
        for host in [
            "rtmp://publisher.example.test",
            "publisher.example.test/live/1A",
            "publisher.example.test?x=1",
            "publisher.example.test#fragment",
            "user@publisher.example.test",
        ] {
            let mut config = Config::default();
            config.publisher.hosts = vec![host.to_owned()];
            assert!(config.validate().is_err(), "受理してはいけない: {host}");
        }
    }

    #[test]
    fn rejects_publisher_host_with_zero_port() {
        let mut config = Config::default();
        config.publisher.hosts = vec!["publisher.example.test:0".to_owned()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn disables_edamame_without_publisher_host() {
        let mut config = Config::default();
        config.publisher.hosts = vec!["  ".to_owned()];
        assert!(!config.edamame_enabled());
    }

    #[test]
    fn relay_url_uses_bind_and_channel() {
        let config = Config::default();
        assert_eq!(config.relay_server(), "rtmp://127.0.0.1:1935/live");
        assert_eq!(config.relay_url(), "rtmp://127.0.0.1:1935/live/uni");

        let config = Config {
            channel_id: "1A".to_owned(),
            relay: RelayConfig {
                bind: "0.0.0.0:4001".to_owned(),
            },
            ..Config::default()
        };
        assert_eq!(config.relay_url(), "rtmp://0.0.0.0:4001/live/1A");
    }

    #[test]
    fn rejects_relay_bind_with_zero_port() {
        let mut config = Config::default();
        config.relay.bind = "127.0.0.1:0".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn builds_key_uri_from_base_url_and_channel() {
        let config = fallback_config();
        assert_eq!(
            config.imageflux_key_uri().expect("鍵 URI を組み立てられる"),
            "https://live.example.test/key/1A?metric=100000"
        );
    }

    #[test]
    fn strips_trailing_slash_from_base_url() {
        let mut config = fallback_config();
        config.stream_credentials.base_url = "https://live.example.test/".to_owned();
        assert_eq!(
            config.imageflux_key_uri().expect("鍵 URI を組み立てられる"),
            "https://live.example.test/key/1A?metric=100000"
        );
    }

    #[test]
    fn disables_key_uri_without_stream_credentials() {
        assert_eq!(
            Config::default()
                .imageflux_key_uri()
                .expect("鍵 URI を組み立てられる"),
            ""
        );
    }

    #[test]
    fn rejects_invalid_stream_credentials_base_url() {
        for base_url in [
            "http://live.example.test",
            "https://live.example.test/base",
            "https://live.example.test?x=1",
            "https://live.example.test#fragment",
            "not-a-url",
        ] {
            let mut config = fallback_config();
            config.stream_credentials.base_url = base_url.to_owned();
            assert!(config.validate().is_err(), "受理してはいけない: {base_url}");
        }
    }

    #[test]
    fn rejects_partial_stream_credentials() {
        let mut config = fallback_config();
        config.stream_credentials.ingest_token.clear();
        assert!(config.validate().is_err());

        let mut config = fallback_config();
        config.stream_credentials.base_url.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn requires_imageflux_token_for_fallback() {
        let mut config = fallback_config();
        config.imageflux.token.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_ingest_token_with_spaces() {
        let mut config = fallback_config();
        config.stream_credentials.ingest_token = "bad token".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn saves_config_file_with_secrets() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("現在時刻を取得できる")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tomyou-config-{}-{suffix}.toml",
            std::process::id()
        ));
        let config = Config {
            obs: ObsConfig {
                password: "obs-secret".to_owned(),
                ..ObsConfig::default()
            },
            stream_credentials: StreamCredentialsConfig {
                base_url: "https://live.example.test".to_owned(),
                ingest_token: "ingest-secret".to_owned(),
                ..StreamCredentialsConfig::default()
            },
            publisher: PublisherConfig {
                hosts: vec!["publisher.example.test".to_owned()],
            },
            imageflux: ImageFluxConfig {
                token: "imageflux-secret".to_owned(),
                ..ImageFluxConfig::default()
            },
            ..Config::default()
        };
        config.save(&path).expect("設定を保存できる");
        let text = fs::read_to_string(&path).expect("保存した設定を読める");
        assert!(text.contains("obs-secret"));
        assert!(text.contains("imageflux-secret"));
        assert!(text.contains("ingest-secret"));
        assert!(text.contains("publisher.example.test"));
        let loaded = Config::load(&path).expect("保存した設定を読み込める");
        assert_eq!(loaded.publisher.hosts, vec!["publisher.example.test"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)
                .expect("設定ファイルの情報を取得できる")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_file(&path).expect("設定ファイルを消せる");
    }

    #[test]
    fn rejects_old_publisher_url_keys() {
        let text = r#"
channel_id = "1A"

[publisher]
rtmp_urls = ["rtmp://publisher.example.test/live/1A"]
"#;
        let error = toml::from_str::<Config>(text).expect_err("旧キーは拒否する");
        assert!(
            error.to_string().contains("rtmp_urls"),
            "旧キー名をエラーに含める: {error}"
        );
    }

    #[test]
    fn normalizes_channel_id_case() {
        let mut config = Config {
            channel_id: "1a".to_owned(),
            ..Config::default()
        };
        config.normalize_channel_id();
        assert_eq!(config.channel_id, "1A");
        config.validate().expect("正規化後は有効");
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
hosts = ["publisher.example.test"]

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
