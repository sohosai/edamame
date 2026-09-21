//! publisher の CLI と TOML 設定
//!
//! 入力と rendition は TOML に書き、実行ごとに変わる controller の接続先は CLI で渡す。

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

    /// controller の絶対 http URL
    #[arg(long)]
    pub controller_url: String,

    /// PUT の認証に使う Bearer トークン
    #[arg(long)]
    pub auth_token: Option<String>,

    /// セグメントを作る作業ディレクトリ。省略すると一時ディレクトリを使う
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// 暗号鍵を読み書きするファイル。省略すると実行ごとに生成する
    #[arg(long)]
    pub key_file: Option<PathBuf>,

    /// live2025-server のベース URL。指定すると鍵と配信 URL を登録する
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
    pub controller_url: String,
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
    // TOML では [[input]] と書く
    #[serde(rename = "input")]
    inputs: Vec<InputSettings>,
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
    pub srt_url: String,
    /// controller の公開マスタープレイリストの URL。鍵と一緒に登録する
    pub live_url: Option<String>,
    /// メディアプレイリストに出す鍵 URI
    pub key_uri: Option<String>,
    // TOML では [[input.rendition]] と書く
    #[serde(rename = "rendition")]
    pub renditions: Vec<RenditionSettings>,
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
    /// エンコーダの目標ビットレート。mode が copy のときは指定しない
    pub video_bitrate: Option<u64>,
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
    /// `WIDTHxHEIGHT` を読み取る
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
        // yuv420p で割り切れない幅と高さは scale filter が扱えないため、起動時に弾く
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
    /// 設定ファイルと CLI を読み込み、検証して返す
    pub fn load(cli: &Cli) -> Result<Self, String> {
        let text = fs::read_to_string(&cli.config)
            .map_err(|error| format!("failed to read {}: {error}", cli.config.display()))?;
        let file: FileSettings = toml::from_str(&text)
            .map_err(|error| format!("failed to parse {}: {error}", cli.config.display()))?;
        let controller_url = normalize_controller_url(&cli.controller_url)?;
        validate_auth_token(cli.auth_token.as_deref())?;
        validate_auth_token(cli.stream_ingest_token.as_deref())?;
        let settings = Self {
            segment_duration_ms: file.segment_duration_ms,
            backlog_limit: file.backlog_limit,
            encoder: file.encoder,
            controller_url,
            auth_token: cli.auth_token.clone(),
            key_file: cli.key_file.clone(),
            stream_credentials_url: cli
                .stream_credentials_url
                .as_deref()
                .map(|url| normalize_base_url("stream_credentials_url", url))
                .transpose()?,
            stream_ingest_token: cli.stream_ingest_token.clone(),
            inputs: file.inputs,
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
            if !input.srt_url.starts_with("srt://") {
                return Err(format!(
                    "srt_url must start with srt://: channel {}",
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
            if self.stream_credentials_url.is_some() && input.live_url.is_none() {
                return Err(format!(
                    "live_url is required when stream_credentials_url is set: channel {}",
                    input.channel_id
                ));
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
                        if rendition.video_bitrate.is_some() {
                            return Err(format!(
                                "video_bitrate must not be set when mode is copy: channel {} quality {}",
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
    if url.scheme() != "http" {
        return Err("controller URL must start with http://".to_owned());
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

/// 絶対 URL を検証する。`https_only` が真なら https だけを受け付ける
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
            controller_url: "http://127.0.0.1:9000".to_owned(),
            auth_token: None,
            key_file: None,
            stream_credentials_url: None,
            stream_ingest_token: None,
            inputs: vec![InputSettings {
                channel_id: "1A".to_owned(),
                srt_url: "srt://0.0.0.0:9101?mode=listener".to_owned(),
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
        second.srt_url = "srt://0.0.0.0:9102?mode=listener".to_owned();
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
    fn rejects_video_bitrate_for_copy() {
        let mut target = settings();
        target.inputs[0].renditions[0].video_bitrate = Some(1_000_000);
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_encode_without_video_bitrate() {
        let mut target = settings();
        target.inputs[0].renditions[0].mode = RenditionMode::Encode;
        assert!(target.validate().is_err());
    }

    #[test]
    fn rejects_non_srt_url() {
        let mut target = settings();
        target.inputs[0].srt_url = "http://127.0.0.1:9101".to_owned();
        assert!(target.validate().is_err());
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
    fn requires_token_and_live_url_for_registration() {
        let mut target = settings();
        target.stream_credentials_url = Some("https://live.example.test".to_owned());
        // トークンがない
        assert!(target.validate().is_err());
        target.stream_ingest_token = Some("ingest-token".to_owned());
        // live_url がない
        assert!(target.validate().is_err());
        target.inputs[0].live_url =
            Some("https://live.example.test/channels/1A/master.m3u8".to_owned());
        target.validate().unwrap();
    }

    #[test]
    fn rejects_token_without_registration_url() {
        let mut target = settings();
        target.stream_ingest_token = Some("ingest-token".to_owned());
        assert!(target.validate().is_err());
    }
}
