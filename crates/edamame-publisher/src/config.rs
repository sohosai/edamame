//! publisher の CLI と TOML 設定
//!
//! 入力と rendition は TOML に書き、controller の接続先は TOML の [[controller]] または CLI で渡す。

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

/// 起動時のコマンドライン引数
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-publisher", about = "Edamame publisher")]
pub struct Cli {
    /// TOML 設定ファイルのパス
    #[arg(long)]
    pub config: PathBuf,

    /// controller の接続先を表す http の絶対 URL。
    /// 1 台だけ指定する後方互換用。複数台は TOML の [[controller]] を使う
    #[arg(long)]
    pub controller_url: Option<String>,

    /// PUT の認証に使う Bearer トークン
    #[arg(long)]
    pub auth_token: Option<String>,

    /// 作業ディレクトリ `edamame-publisher-<pid>` を作る親ディレクトリ。
    /// 省略すると一時ディレクトリの配下に作る
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// 暗号鍵を読み書きするファイル。
    /// 省略すると実行ごとに鍵を生成する
    #[arg(long)]
    pub key_file: Option<PathBuf>,

    /// 鍵と配信 URL を登録する live2025-server のベース URL。
    /// 指定時は --stream-ingest-token と全 input の live_url が必要
    #[arg(long)]
    pub stream_credentials_url: Option<String>,

    /// live2025-server の STREAM_INGEST_TOKEN と同じトークン
    #[arg(long)]
    pub stream_ingest_token: Option<String>,
}

/// 検証済みの設定
#[derive(Debug, Clone)]
pub struct PublisherSettings {
    pub segment_duration_ms: u64,
    pub backlog_limit: u64,
    pub encoder: EncoderSettings,
    pub controllers: Vec<ControllerSettings>,
    pub auth_token: Option<String>,
    pub key_file: Option<PathBuf>,
    pub stream_credentials_url: Option<String>,
    pub stream_ingest_token: Option<String>,
    pub inputs: Vec<InputSettings>,
}

/// TOML に書く設定
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSettings {
    segment_duration_ms: u64,
    backlog_limit: u64,
    encoder: EncoderSettings,
    // TOML では [[controller]] と書く
    #[serde(rename = "controller", default)]
    controllers: Vec<ControllerSettings>,
    // TOML では [[input]] と書く
    #[serde(rename = "input")]
    inputs: Vec<InputSettings>,
}

/// controller の接続先と優先度
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerSettings {
    /// controller の http(s) の絶対 URL
    pub url: String,
    /// 小さいほど優先する。チャンネル内で重複不可
    pub metric: u32,
}

impl ControllerSettings {
    /// この controller が配信するチャンネルのマスタープレイリスト URL
    #[must_use]
    pub fn live_url(&self, channel_id: &str) -> String {
        format!("{}/channels/{channel_id}/master.m3u8", self.url)
    }
}

/// エンコーダの指定
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderSettings {
    /// ffmpeg に渡す映像エンコーダの名前
    pub codec: String,
    /// ffmpeg に渡すプリセット
    pub preset: String,
}

/// 1 つの channel の入力
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSettings {
    pub channel_id: String,
    /// ffmpeg に渡す入力 URL。srt:// は URL の mode で待ち受け、rtmp:// は publisher が待ち受ける
    pub input_url: String,
    /// 先頭 controller の公開マスタープレイリスト URL の上書き。
    /// 未指定なら controller の URL から導出する
    pub live_url: Option<String>,
    /// メディアプレイリストに出力する暗号鍵の URI
    pub key_uri: Option<String>,
    // TOML では [[input.rendition]] と書く
    #[serde(rename = "rendition")]
    pub renditions: Vec<RenditionSettings>,
}

impl InputSettings {
    /// RTMP の入力 URL にチャンネル別のポートと /live/<channel_id> を補う
    fn normalize_input_url(&mut self) -> Result<(), String> {
        let text = self.input_url.trim();
        if !text.starts_with("rtmp://") {
            // SRT は URL に待ち受け方法が含まれるため、そのまま使う
            return Ok(());
        }
        let url = reqwest::Url::parse(text).map_err(|error| {
            format!("invalid input_url for channel {}: {error}", self.channel_id)
        })?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!(
                "input_url must not contain credentials: channel {}",
                self.channel_id
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(format!(
                "input_url must not contain a query or fragment: channel {}",
                self.channel_id
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| format!("input_url must contain a host: channel {}", self.channel_id))?;
        let port = match url.port() {
            Some(0) => {
                return Err(format!(
                    "input_url port must be 1 or greater: channel {}",
                    self.channel_id
                ));
            }
            Some(port) => port,
            None => channel_port(&self.channel_id).ok_or_else(|| {
                format!(
                    "channel {} has no default RTMP port; specify host:port in input_url",
                    self.channel_id
                )
            })?,
        };
        let expected_path = format!("/live/{}", self.channel_id);
        let path = match url.path() {
            "" | "/" => expected_path,
            path if path == expected_path || path == format!("{expected_path}/") => expected_path,
            _ => {
                return Err(format!(
                    "input_url path must be {expected_path} or omitted: channel {}",
                    self.channel_id
                ));
            }
        };
        self.input_url = format!("rtmp://{host}:{port}{path}");
        Ok(())
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

/// 1 つの quality の作り方
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenditionSettings {
    pub quality: String,
    #[serde(default)]
    pub mode: RenditionMode,
    pub resolution: Resolution,
    /// controller へ宣言するビットレート
    pub bandwidth: u64,
    /// エンコーダの目標ビットレート。
    /// mode が copy のときは任意で、入力の解像度が合わずに encode へ切り替えたときに使う
    pub video_bitrate: Option<u64>,
}

impl RenditionSettings {
    /// copy の入力が設定と合わないときに代わりに使う encode の設定を返す。
    /// video_bitrate がなければ bandwidth を目標ビットレートにする
    #[must_use]
    pub fn as_encode_fallback(&self) -> Self {
        Self {
            mode: RenditionMode::Encode,
            video_bitrate: Some(self.video_bitrate.unwrap_or(self.bandwidth)),
            ..self.clone()
        }
    }
}

/// rendition の作り方
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenditionMode {
    /// 入力の映像をそのままコピーする
    Copy,
    /// 指定したエンコーダで作り直す
    #[default]
    Encode,
}

/// 映像の解像度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    /// `WIDTHxHEIGHT` 形式の文字列を解像度として読み取る
    fn parse(text: &str) -> Result<Self, String> {
        let (width_text, height_text) = text
            .split_once('x')
            .ok_or_else(|| format!("resolution must be WIDTHxHEIGHT: {text}"))?;
        let width: u32 = width_text
            .parse()
            .map_err(|_| format!("resolution width must be a number: {text}"))?;
        let height: u32 = height_text
            .parse()
            .map_err(|_| format!("resolution height must be a number: {text}"))?;
        // mode にかかわらず、幅と高さはそれぞれ 2 以上の偶数だけを受け付ける
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!(
                "resolution must be even numbers of at least 2: {text}"
            ));
        }
        Ok(Self { width, height })
    }

    /// `WIDTHxHEIGHT` の文字列を返す
    #[must_use]
    pub fn text(self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

impl<'de> Deserialize<'de> for Resolution {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl PublisherSettings {
    /// 設定ファイルと CLI から設定を読み込み、検証済みの設定を返す
    pub fn load(cli: &Cli) -> Result<Self, String> {
        let text = fs::read_to_string(&cli.config)
            .map_err(|error| format!("failed to read {}: {error}", cli.config.display()))?;
        let file: FileSettings = toml::from_str(&text)
            .map_err(|error| format!("failed to parse {}: {error}", cli.config.display()))?;
        let cli_controller_url = cli
            .controller_url
            .as_deref()
            .map(normalize_controller_url)
            .transpose()?;
        if !file.controllers.is_empty() && cli_controller_url.is_some() {
            return Err("--controller-url and [[controller]] must not be combined".to_owned());
        }
        let mut controllers = file.controllers;
        if controllers.is_empty() {
            let Some(url) = cli_controller_url else {
                return Err(
                    "controller URL is required: pass --controller-url or [[controller]]"
                        .to_owned(),
                );
            };
            controllers.push(ControllerSettings { url, metric: 0 });
        } else {
            for controller in &mut controllers {
                controller.url = normalize_controller_url(&controller.url)?;
            }
        }
        controllers.sort_by(|left, right| {
            left.metric
                .cmp(&right.metric)
                .then_with(|| left.url.cmp(&right.url))
        });
        validate_auth_token(cli.auth_token.as_deref())?;
        validate_auth_token(cli.stream_ingest_token.as_deref())?;
        let mut inputs = file.inputs;
        for input in &mut inputs {
            input.normalize_input_url()?;
        }
        let settings = Self {
            segment_duration_ms: file.segment_duration_ms,
            backlog_limit: file.backlog_limit,
            encoder: file.encoder,
            controllers,
            auth_token: cli.auth_token.clone(),
            key_file: cli.key_file.clone(),
            stream_credentials_url: cli
                .stream_credentials_url
                .as_deref()
                .map(|url| normalize_base_url("stream_credentials_url", url))
                .transpose()?,
            stream_ingest_token: cli.stream_ingest_token.clone(),
            inputs,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// 設定の矛盾を起動時に見つける
    fn validate(&self) -> Result<(), String> {
        if self.segment_duration_ms < 500 {
            return Err("segment_duration_ms must be at least 500".to_owned());
        }
        if self.backlog_limit == 0 {
            return Err("backlog_limit must be positive".to_owned());
        }
        if self.encoder.codec.trim().is_empty() {
            return Err("encoder.codec must not be empty".to_owned());
        }
        if self.encoder.preset.trim().is_empty() {
            return Err("encoder.preset must not be empty".to_owned());
        }
        if self.inputs.is_empty() {
            return Err("at least one input is required".to_owned());
        }
        if self.controllers.is_empty() {
            return Err("at least one controller is required".to_owned());
        }
        let mut controller_metrics = HashSet::new();
        let mut controller_urls = HashSet::new();
        for controller in &self.controllers {
            if controller.metric > i32::MAX as u32 {
                return Err("controller metric must fit in a signed 32-bit integer".to_owned());
            }
            if !controller_metrics.insert(controller.metric) {
                return Err("controller metric is duplicated".to_owned());
            }
            if !controller_urls.insert(controller.url.as_str()) {
                return Err("controller URL is duplicated".to_owned());
            }
        }
        if self.stream_credentials_url.is_some() && self.stream_ingest_token.is_none() {
            return Err(
                "stream_ingest_token is required when stream_credentials_url is set".to_owned(),
            );
        }
        if self.stream_credentials_url.is_none() && self.stream_ingest_token.is_some() {
            return Err(
                "stream_credentials_url is required when stream_ingest_token is set".to_owned(),
            );
        }

        let mut channel_ids = HashSet::new();
        for input in &self.inputs {
            validate_identifier("channel_id", &input.channel_id)?;
            if !channel_ids.insert(input.channel_id.as_str()) {
                return Err(format!("channel_id is duplicated: {}", input.channel_id));
            }
            if !input.input_url.starts_with("srt://") && !input.input_url.starts_with("rtmp://") {
                return Err(format!(
                    "input_url must start with srt:// or rtmp://: channel {}",
                    input.channel_id
                ));
            }
            if input.renditions.is_empty() {
                return Err(format!(
                    "channel {} must have at least one rendition",
                    input.channel_id
                ));
            }
            if let Some(live_url) = &input.live_url {
                validate_url("live_url", live_url, true)?;
            }
            if let Some(key_uri) = &input.key_uri {
                validate_url("key_uri", key_uri, false)?;
            }

            let mut qualities = HashSet::new();
            for rendition in &input.renditions {
                validate_identifier("quality", &rendition.quality)?;
                if rendition.quality == "master" {
                    return Err(format!(
                        "quality must not be master: channel {}",
                        input.channel_id
                    ));
                }
                if !qualities.insert(rendition.quality.as_str()) {
                    return Err(format!(
                        "quality is duplicated: channel {} quality {}",
                        input.channel_id, rendition.quality
                    ));
                }
                if rendition.bandwidth == 0 {
                    return Err(format!(
                        "bandwidth must be positive: channel {} quality {}",
                        input.channel_id, rendition.quality
                    ));
                }
                match rendition.mode {
                    RenditionMode::Copy => {
                        if rendition.video_bitrate == Some(0) {
                            return Err(format!(
                                "video_bitrate must be positive when set: channel {} quality {}",
                                input.channel_id, rendition.quality
                            ));
                        }
                    }
                    RenditionMode::Encode => {
                        if rendition.video_bitrate.unwrap_or(0) == 0 {
                            return Err(format!(
                                "video_bitrate must be positive when mode is encode: channel {} quality {}",
                                input.channel_id, rendition.quality
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 入力に対する controller ごとの live_url を、優先度の高い順に返す。
    /// input.live_url は先頭 controller の上書きとしてだけ使う。
    #[must_use]
    pub fn candidate_live_urls(&self, input: &InputSettings) -> Vec<(u32, String)> {
        self.controllers
            .iter()
            .enumerate()
            .map(|(index, controller)| {
                let live_url = if index == 0 {
                    input
                        .live_url
                        .clone()
                        .unwrap_or_else(|| controller.live_url(&input.channel_id))
                } else {
                    controller.live_url(&input.channel_id)
                };
                (controller.metric, live_url)
            })
            .collect()
    }
}

/// 識別子を edamame と同じ規則で検証する
fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 {
        return Err(format!("{name} must be 1 to 64 characters: {value}"));
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(format!("{name} must contain only [A-Za-z0-9_-]: {value}"));
    }
    Ok(())
}

/// controller の URL を検証し、末尾の `/` を取り除く
fn normalize_controller_url(text: &str) -> Result<String, String> {
    let url =
        reqwest::Url::parse(text).map_err(|error| format!("invalid controller URL: {error}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("controller URL must start with http:// or https://".to_owned());
    }
    if url.host().is_none() {
        return Err("controller URL must contain a host".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("controller URL must not contain a query or fragment".to_owned());
    }
    Ok(text.trim_end_matches('/').to_owned())
}

/// HTTP ヘッダーに載せられるトークンだけを受け付ける
fn validate_auth_token(token: Option<&str>) -> Result<(), String> {
    let Some(token) = token else {
        return Ok(());
    };
    if token.is_empty()
        || !token
            .chars()
            .all(|character| ('!'..='~').contains(&character))
    {
        return Err("auth_token must be non-empty printable ASCII without spaces".to_owned());
    }
    Ok(())
}

/// ベース URL を検証し、末尾の `/` を取り除く
fn normalize_base_url(name: &str, text: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(text).map_err(|error| format!("invalid {name}: {error}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("{name} must start with http:// or https://"));
    }
    if url.host().is_none() {
        return Err(format!("{name} must contain a host"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{name} must not contain a query or fragment"));
    }
    Ok(text.trim_end_matches('/').to_owned())
}

/// 絶対 URL を検証する。
/// `https_only` が真なら https だけを受け付ける
fn validate_url(name: &str, text: &str, https_only: bool) -> Result<(), String> {
    let url = reqwest::Url::parse(text).map_err(|_| format!("{name} must be an absolute URL"))?;
    let scheme_allowed = if https_only {
        url.scheme() == "https"
    } else {
        url.scheme() == "http" || url.scheme() == "https"
    };
    if !scheme_allowed {
        return Err(format!(
            "{name} must be {}",
            if https_only { "https" } else { "http or https" }
        ));
    }
    if url.host().is_none() {
        return Err(format!("{name} must contain a host"));
    }
    if url.fragment().is_some() {
        return Err(format!("{name} must not contain a fragment"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 検証を通る最小の設定
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
                renditions: vec![RenditionSettings {
                    quality: "1080p".to_owned(),
                    mode: RenditionMode::Copy,
                    resolution: Resolution {
                        width: 1920,
                        height: 1080,
                    },
                    bandwidth: 12_000_000,
                    video_bitrate: None,
                }],
            }],
        }
    }

    #[test]
    fn accepts_valid_settings() {
        settings().validate().unwrap();
    }

    #[test]
    fn rejects_short_segment_duration() {
        let mut target = settings();
        target.segment_duration_ms = 499;
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_duplicated_channel_id() {
        let mut target = settings();
        let mut second = target.inputs[0].clone();
        second.input_url = "srt://0.0.0.0:9102?mode=listener".to_owned();
        target.inputs.push(second);
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_duplicated_quality_in_a_channel() {
        let mut target = settings();
        let second = target.inputs[0].renditions[0].clone();
        target.inputs[0].renditions.push(second);
        assert!(target.validate().is_err());
    }

    #[test]
    fn accepts_positive_video_bitrate_for_copy() {
        let mut target = settings();
        target.inputs[0].renditions[0].video_bitrate = Some(1_000_000);
        target.validate().unwrap();
        target.inputs[0].renditions[0].video_bitrate = Some(0);
        assert!(target.validate().is_err());
    }

    #[test]
    fn builds_encode_fallback_for_copy() {
        let mut rendition = settings().inputs[0].renditions[0].clone();
        let fallback = rendition.as_encode_fallback();
        assert_eq!(fallback.mode, RenditionMode::Encode);
        assert_eq!(fallback.video_bitrate, Some(12_000_000));
        assert_eq!(fallback.resolution, rendition.resolution);

        rendition.video_bitrate = Some(8_000_000);
        assert_eq!(
            rendition.as_encode_fallback().video_bitrate,
            Some(8_000_000)
        );
    }

    #[test]
    fn rejects_encode_without_video_bitrate() {
        let mut target = settings();
        target.inputs[0].renditions[0].mode = RenditionMode::Encode;
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_input_url_with_other_scheme() {
        let mut target = settings();
        target.inputs[0].input_url = "http://127.0.0.1:9101".to_owned();
        assert!(target.validate().is_err());
        target.inputs[0].input_url = "rtmps://0.0.0.0:1935/live/1A".to_owned();
        assert!(target.validate().is_err());
    }

    #[test]
    fn accepts_rtmp_input_url() {
        let mut target = settings();
        target.inputs[0].input_url = "rtmp://0.0.0.0:1936/live/1A".to_owned();
        target.validate().unwrap();
    }

    /// 入力 URL を正規化した結果を返す
    fn normalized_input(channel_id: &str, input_url: &str) -> Result<String, String> {
        let mut input = settings().inputs[0].clone();
        input.channel_id = channel_id.to_owned();
        input.input_url = input_url.to_owned();
        input.normalize_input_url()?;
        Ok(input.input_url)
    }

    #[test]
    fn channel_ports_follow_the_deployment_table() {
        assert_eq!(channel_port("uni"), Some(1935));
        assert_eq!(channel_port("1A"), Some(1936));
        assert_eq!(channel_port("kaikan"), Some(1937));
        assert_eq!(channel_port("1B"), None);
    }

    #[test]
    fn parses_and_normalizes_the_example_config() {
        let file: FileSettings =
            toml::from_str(include_str!("../publisher.example.toml")).expect("設定例を読める");
        let mut inputs = file.inputs;
        for input in &mut inputs {
            input
                .normalize_input_url()
                .expect("設定例の入力 URL を補完できる");
        }
        let input_1a = inputs
            .iter()
            .find(|input| input.channel_id == "1A")
            .expect("1A の設定がある");
        assert_eq!(input_1a.input_url, "rtmp://0.0.0.0:1936/live/1A");
    }

    #[test]
    fn fills_rtmp_port_and_path_for_a_known_channel() {
        assert_eq!(
            normalized_input("1A", "rtmp://0.0.0.0").expect("ポートを補完できる"),
            "rtmp://0.0.0.0:1936/live/1A"
        );
        assert_eq!(
            normalized_input("uni", "rtmp://0.0.0.0/").expect("ポートを補完できる"),
            "rtmp://0.0.0.0:1935/live/uni"
        );
    }

    #[test]
    fn fills_rtmp_port_for_an_ipv6_host() {
        assert_eq!(
            normalized_input("1A", "rtmp://[::1]").expect("IPv6 のホストを使える"),
            "rtmp://[::1]:1936/live/1A"
        );
    }

    #[test]
    fn load_fills_the_default_rtmp_port() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("現在時刻を取得できる")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "edamame-publisher-{}-{suffix}.toml",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"
segment_duration_ms = 6000
backlog_limit = 6

[encoder]
codec = "libx264"
preset = "ultrafast"

[[input]]
channel_id = "1A"
input_url = "rtmp://127.0.0.1"

[[input.rendition]]
quality = "main"
mode = "copy"
resolution = "640x360"
bandwidth = 1000000
"#,
        )
        .expect("設定を書ける");
        let cli = Cli {
            config: path.clone(),
            controller_url: Some("http://127.0.0.1:9000".to_owned()),
            auth_token: None,
            work_dir: None,
            key_file: None,
            stream_credentials_url: None,
            stream_ingest_token: None,
        };
        let settings = PublisherSettings::load(&cli).expect("設定を読み込める");
        assert_eq!(
            settings.inputs[0].input_url,
            "rtmp://127.0.0.1:1936/live/1A"
        );
        fs::remove_file(&path).expect("設定を消せる");
    }

    #[test]
    fn explicit_rtmp_port_overrides_the_channel_port() {
        assert_eq!(
            normalized_input("1A", "rtmp://0.0.0.0:4001").expect("明示ポートを使える"),
            "rtmp://0.0.0.0:4001/live/1A"
        );
    }

    #[test]
    fn keeps_a_matching_rtmp_path() {
        assert_eq!(
            normalized_input("1B", "rtmp://0.0.0.0:4001/live/1B").expect("一致するパスを使える"),
            "rtmp://0.0.0.0:4001/live/1B"
        );
        assert_eq!(
            normalized_input("1B", "rtmp://0.0.0.0:4001/live/1B/").expect("末尾スラッシュを使える"),
            "rtmp://0.0.0.0:4001/live/1B"
        );
    }

    #[test]
    fn rejects_rtmp_path_for_another_channel() {
        assert!(normalized_input("1A", "rtmp://0.0.0.0:4001/live/1B").is_err());
    }

    #[test]
    fn rejects_rtmp_without_a_port_for_an_unknown_channel() {
        assert!(normalized_input("1B", "rtmp://0.0.0.0").is_err());
    }

    #[test]
    fn rejects_rtmp_with_invalid_port_or_credentials() {
        assert!(normalized_input("1A", "rtmp://0.0.0.0:0").is_err());
        assert!(normalized_input("1A", "rtmp://user:pass@0.0.0.0:4001").is_err());
        assert!(normalized_input("1A", "rtmp://0.0.0.0:4001?x=1").is_err());
        assert!(normalized_input("1A", "rtmp://0.0.0.0:4001#fragment").is_err());
    }

    #[test]
    fn leaves_srt_input_url_unchanged() {
        assert_eq!(
            normalized_input("1A", "srt://0.0.0.0:9101?mode=listener")
                .expect("SRT はそのまま使える"),
            "srt://0.0.0.0:9101?mode=listener"
        );
    }

    #[test]
    fn parses_resolution_text() {
        let resolution = Resolution::parse("854x480").unwrap();
        assert_eq!(resolution.text(), "854x480");
    }

    #[test]
    fn rejects_odd_resolution() {
        assert!(Resolution::parse("853x480").is_err());
    }

    #[test]
    fn normalizes_controller_url() {
        let url = normalize_controller_url("http://127.0.0.1:9000/").unwrap();
        assert_eq!(url, "http://127.0.0.1:9000");
    }

    #[test]
    fn accepts_https_controller_url() {
        let url = normalize_controller_url("https://edamame-controller.sohosai.com/").unwrap();
        assert_eq!(url, "https://edamame-controller.sohosai.com");
        assert!(normalize_controller_url("ftp://edamame-controller.sohosai.com").is_err());
    }

    #[test]
    fn rejects_controller_url_with_query() {
        assert!(normalize_controller_url("http://127.0.0.1:9000/?x=1").is_err());
    }

    #[test]
    fn rejects_auth_token_with_space() {
        assert!(validate_auth_token(Some("token with space")).is_err());
        assert!(validate_auth_token(Some("example-token")).is_ok());
    }

    #[test]
    fn rejects_live_url_that_is_not_https() {
        let mut target = settings();
        target.inputs[0].live_url =
            Some("http://live.example.test/channels/1A/master.m3u8".to_owned());
        assert!(target.validate().is_err());
        target.inputs[0].live_url =
            Some("https://live.example.test/channels/1A/master.m3u8".to_owned());
        target.validate().unwrap();
    }

    #[test]
    fn rejects_key_uri_with_fragment() {
        let mut target = settings();
        target.inputs[0].key_uri = Some("https://live.example.test/key/1A#fragment".to_owned());
        assert!(target.validate().is_err());
        target.inputs[0].key_uri = Some("https://live.example.test/key/1A".to_owned());
        target.validate().unwrap();
    }

    #[test]
    fn requires_token_for_registration() {
        let mut target = settings();
        target.stream_credentials_url = Some("https://live.example.test".to_owned());
        // トークンがない
        assert!(target.validate().is_err());
        target.stream_ingest_token = Some("ingest-token".to_owned());
        target.validate().unwrap();
    }

    #[test]
    fn parses_controller_settings() {
        let controller: ControllerSettings =
            toml::from_str("url = \"https://controller.example.test\"\nmetric = 100").unwrap();
        assert_eq!(controller.url, "https://controller.example.test");
        assert_eq!(controller.metric, 100);
    }

    #[test]
    fn derives_candidate_live_urls_in_metric_order() {
        let mut target = settings();
        target.controllers = vec![
            ControllerSettings {
                url: "https://a.example.test".to_owned(),
                metric: 0,
            },
            ControllerSettings {
                url: "https://b.example.test".to_owned(),
                metric: 1,
            },
        ];
        let candidates = target.candidate_live_urls(&target.inputs[0]);
        assert_eq!(
            candidates,
            vec![
                (
                    0,
                    "https://a.example.test/channels/1A/master.m3u8".to_owned()
                ),
                (
                    1,
                    "https://b.example.test/channels/1A/master.m3u8".to_owned()
                ),
            ]
        );

        target.inputs[0].live_url = Some("https://override.example.test/master.m3u8".to_owned());
        let candidates = target.candidate_live_urls(&target.inputs[0]);
        assert_eq!(candidates[0].1, "https://override.example.test/master.m3u8");
        assert_eq!(
            candidates[1].1,
            "https://b.example.test/channels/1A/master.m3u8"
        );
    }

    #[test]
    fn rejects_duplicated_controller_metric() {
        let mut target = settings();
        target.controllers.push(ControllerSettings {
            url: "http://127.0.0.1:9001".to_owned(),
            metric: 1,
        });
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_duplicated_controller_url() {
        let mut target = settings();
        target.controllers.push(ControllerSettings {
            url: "http://127.0.0.1:9000".to_owned(),
            metric: 2,
        });
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_token_without_registration_url() {
        let mut target = settings();
        target.stream_ingest_token = Some("ingest-token".to_owned());
        assert!(target.validate().is_err());
    }
}
