//! ImageFlux Live Streaming API のクライアント

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};
use url::Url;

use crate::config::{HlsArchive, HlsAudio, HlsRendition, HlsVideo};

const API_URL: &str = "https://live-api.imageflux.jp/";

/// ImageFlux の API クライアント
#[derive(Clone)]
pub struct ImageFluxClient {
    http: Client,
    token: String,
}

/// 作成したチャンネル
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedChannel {
    pub channel_id: String,
    pub sora_url: String,
    pub playlist_url: Option<String>,
}

impl ImageFluxClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("ImageFlux 用の HTTP クライアントを作れません")?;
        Ok(Self {
            http,
            token: token.into(),
        })
    }

    /// HLS 付きのマルチストリームチャンネルを作る
    pub async fn create_channel(
        &self,
        hls: &[HlsRendition],
        encrypt_key_uri: &str,
        event_webhook_url: &str,
    ) -> Result<CreatedChannel> {
        let request = CreateChannelRequest {
            hls: hls.iter().map(CreateHls::from).collect(),
            encrypt_key_uri: optional(encrypt_key_uri),
            event_webhook_url: optional(event_webhook_url),
        };
        let response = self
            .request(
                "ImageFlux_20200316.CreateMultistreamChannelWithHLS",
                &request,
            )
            .await?;
        let parsed: CreateChannelResponse = serde_json::from_slice(&response)
            .context("ImageFlux のチャンネル作成応答を読めません")?;
        if parsed.ok == Some(false) {
            bail!(
                "ImageFlux のチャンネル作成に失敗しました: {}",
                parsed.error.unwrap_or_default()
            );
        }
        if parsed.channel_id.trim().is_empty() {
            bail!("ImageFlux のチャンネル作成応答に channel_id がありません");
        }
        info!("ImageFlux channel created: {}", parsed.channel_id);
        Ok(CreatedChannel {
            channel_id: parsed.channel_id,
            sora_url: parsed.sora_url.unwrap_or_default(),
            playlist_url: parsed.playlist_url,
        })
    }

    /// チャンネルの HLS プレイリスト URL を取得する
    pub async fn list_playlist_urls(&self, channel_id: &str) -> Result<Vec<String>> {
        let response = self
            .request(
                "ImageFlux_20200207.ListPlaylistURLs",
                &json!({ "channel_id": channel_id }),
            )
            .await?;
        let parsed: ListPlaylistResponse = serde_json::from_slice(&response)
            .context("ImageFlux のプレイリスト応答を読めません")?;
        Ok(parsed
            .hls
            .into_iter()
            .filter_map(|item| item.playlist_url)
            .filter(|url| !url.trim().is_empty())
            .collect())
    }

    /// プレイリストが生成されるまで待つ
    pub async fn wait_for_playlist(&self, channel_id: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.list_playlist_urls(channel_id).await {
                Ok(urls) if !urls.is_empty() => return Ok(urls[0].clone()),
                Ok(_) => {}
                Err(error) => warn!("ImageFlux playlist is not ready yet: {error}"),
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("ImageFlux のプレイリストが 30 秒以内に生成されませんでした");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// チャンネルを削除する
    pub async fn delete_channel(&self, channel_id: &str) -> Result<()> {
        let response = self
            .request(
                "ImageFlux_20180501.DeleteChannel",
                &json!({ "channel_id": channel_id }),
            )
            .await?;
        if response.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let parsed: SimpleResponse =
            serde_json::from_slice(&response).context("ImageFlux の削除応答を読めません")?;
        if parsed.ok == Some(false) {
            bail!(
                "ImageFlux のチャンネル削除に失敗しました: {}",
                parsed.error.unwrap_or_default()
            );
        }
        Ok(())
    }

    async fn request<T: Serialize>(&self, target: &str, body: &T) -> Result<Vec<u8>> {
        let response = self
            .http
            .post(API_URL)
            .header("Content-Type", "application/json")
            .header("X-Sora-Target", target)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await
            .with_context(|| format!("ImageFlux API に接続できません: {target}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("ImageFlux API の応答を読めません")?
            .to_vec();
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            bail!(
                "ImageFlux API が {} を返しました: {}",
                status.as_u16(),
                body.chars().take(300).collect::<String>()
            );
        }
        Ok(bytes)
    }
}

/// Sora の URL から WHIP の送信先を作る
pub fn whip_url(sora_url: &str, channel_id: &str) -> Result<String> {
    let url = Url::parse(sora_url).context("ImageFlux の Sora URL が不正です")?;
    let scheme = match url.scheme() {
        "https" | "wss" => "https",
        other => bail!("ImageFlux の Sora URL の scheme が未対応です: {other}"),
    };
    let host = url
        .host_str()
        .context("ImageFlux の Sora URL にホストがありません")?;
    let port = url.port();
    let authority = match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    Ok(format!("{scheme}://{authority}/whip/{channel_id}"))
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[derive(Serialize)]
struct CreateChannelRequest {
    hls: Vec<CreateHls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypt_key_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_webhook_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateHls {
    duration_seconds: u32,
    start_time_offset: i32,
    video: CreateVideo,
    audio: CreateAudio,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<CreateArchive>,
}

#[derive(Serialize)]
struct CreateVideo {
    width: u32,
    height: u32,
    fps: u32,
    bps: u64,
}

#[derive(Serialize)]
struct CreateAudio {
    bps: u64,
}

#[derive(Serialize)]
struct CreateArchive {
    archive_destination_id: String,
}

impl From<&HlsRendition> for CreateHls {
    fn from(value: &HlsRendition) -> Self {
        let HlsVideo {
            width,
            height,
            fps,
            bps,
        } = value.video;
        let HlsAudio { bps: audio_bps } = value.audio;
        let HlsArchive {
            archive_destination_id,
        } = &value.archive;
        Self {
            duration_seconds: value.duration_seconds,
            start_time_offset: value.start_time_offset,
            video: CreateVideo {
                width,
                height,
                fps,
                bps,
            },
            audio: CreateAudio { bps: audio_bps },
            archive: (!archive_destination_id.trim().is_empty()).then(|| CreateArchive {
                archive_destination_id: archive_destination_id.clone(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateChannelResponse {
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    sora_url: Option<String>,
    #[serde(default)]
    playlist_url: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListPlaylistResponse {
    #[serde(default)]
    hls: Vec<PlaylistItem>,
}

#[derive(Debug, Deserialize)]
struct PlaylistItem {
    #[serde(default)]
    playlist_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SimpleResponse {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HlsRendition;

    #[test]
    fn builds_whip_url_from_https_and_wss() {
        assert_eq!(
            whip_url("https://sora.example.test:443", "abc").unwrap(),
            "https://sora.example.test/whip/abc"
        );
        assert_eq!(
            whip_url("wss://sora.example.test", "abc").unwrap(),
            "https://sora.example.test/whip/abc"
        );
    }

    #[test]
    fn rejects_non_https_sora_url() {
        assert!(whip_url("http://sora.example.test", "abc").is_err());
    }

    #[test]
    fn serializes_hls_camel_case() {
        let rendition = HlsRendition {
            duration_seconds: 2,
            start_time_offset: -2,
            video: crate::config::HlsVideo {
                width: 1920,
                height: 1080,
                fps: 60,
                bps: 15_000_000,
            },
            audio: crate::config::HlsAudio { bps: 320_000 },
            archive: crate::config::HlsArchive::default(),
        };
        let value = serde_json::to_value(CreateHls::from(&rendition)).unwrap();
        assert_eq!(value["durationSeconds"], 2);
        assert_eq!(value["startTimeOffset"], -2);
        assert_eq!(value["video"]["width"], 1920);
        assert!(value.get("archive").is_none());
    }

    #[test]
    fn parses_channel_response() {
        let response: CreateChannelResponse = serde_json::from_str(
            r#"{"channel_id":"ch","sora_url":"wss://sora.example.test","ok":true}"#,
        )
        .unwrap();
        assert_eq!(response.channel_id, "ch");
        assert_eq!(response.ok, Some(true));
    }

    #[test]
    fn parses_empty_delete_response() {
        assert!(serde_json::from_slice::<SimpleResponse>(b"").is_err());
    }
}
