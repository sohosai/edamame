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

    /// プレイリストから ImageFlux の暗号鍵を取得する
    pub async fn fetch_encrypt_key(&self, playlist_url: &str) -> Result<String> {
        let playlist = self.fetch_text(playlist_url).await?;
        // マスターなら最初のバリアント、メディアならそのまま鍵タグを探す
        if let Ok(variant_url) = first_variant_url(&playlist, playlist_url) {
            let media = self.fetch_text(&variant_url).await?;
            let key_uri = find_key_uri(&media)
                .context("ImageFlux のプレイリストに #EXT-X-KEY がありません")?;
            return self.fetch_key_from_uri(&key_uri).await;
        }
        let key_uri = find_key_uri(&playlist)
            .context("ImageFlux のプレイリストに #EXT-X-KEY がありません")?;
        self.fetch_key_from_uri(&key_uri).await
    }

    /// 鍵 URI の kid から ImageFlux の暗号鍵を取得する
    async fn fetch_key_from_uri(&self, key_uri: &str) -> Result<String> {
        let kid = kid_from_key_uri(key_uri)?;
        let response = self
            .request("ImageFlux_20200707.GetEncryptKey", &json!({ "kid": kid }))
            .await?;
        let parsed: EncryptKeyResponse =
            serde_json::from_slice(&response).context("ImageFlux の暗号鍵応答を読めません")?;
        let key = parsed.encrypt_key.trim();
        if key.is_empty() {
            bail!(
                "ImageFlux の暗号鍵を取得できませんでした: {}",
                parsed.error.unwrap_or_default()
            );
        }
        if !is_hex_key(key) {
            bail!("ImageFlux の暗号鍵が 16 バイトの hex ではありません");
        }
        Ok(key.to_owned())
    }

    async fn fetch_text(&self, url: &str) -> Result<String> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("ImageFlux のプレイリストに接続できません: {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("ImageFlux のプレイリストを読めません")?;
        if !status.is_success() {
            bail!(
                "ImageFlux のプレイリストが {} を返しました: {url}",
                status.as_u16()
            );
        }
        Ok(body)
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

/// プレイリストから最初の #EXT-X-KEY の URI を探す
fn find_key_uri(playlist: &str) -> Option<String> {
    for line in playlist.lines() {
        let line = line.trim();
        if !line.starts_with("#EXT-X-KEY") {
            continue;
        }
        // METHOD=NONE など URI を持たない行は読み飛ばす
        let Some(start) = line.find("URI=\"") else {
            continue;
        };
        let rest = &line[start + "URI=\"".len()..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        return Some(rest[..end].to_owned());
    }
    None
}

/// マスタープレイリストから最初のバリアント URL を探す
fn first_variant_url(playlist: &str, playlist_url: &str) -> Result<String> {
    let base = Url::parse(playlist_url).context("ImageFlux のプレイリスト URL が不正です")?;
    let mut expect_variant = false;
    for line in playlist.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-STREAM-INF") {
            expect_variant = true;
            continue;
        }
        if expect_variant && !line.is_empty() && !line.starts_with('#') {
            return base
                .join(line)
                .map(|url| url.to_string())
                .with_context(|| format!("ImageFlux のバリアント URL が不正です: {line}"));
        }
    }
    // #EXT-X-STREAM-INF がなくても、m3u8 を指す行があればバリアントとして扱う
    for line in playlist.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = line.split('?').next().unwrap_or(line);
        if path.ends_with(".m3u8") {
            return base
                .join(line)
                .map(|url| url.to_string())
                .with_context(|| format!("ImageFlux のバリアント URL が不正です: {line}"));
        }
    }
    bail!("ImageFlux のマスタープレイリストにバリアント URL がありません");
}

/// 鍵 URI の kid を取り出す
fn kid_from_key_uri(key_uri: &str) -> Result<String> {
    let url = Url::parse(key_uri).context("ImageFlux の鍵 URI が不正です")?;
    url.query_pairs()
        .find(|(key, _)| key == "kid")
        .map(|(_, value)| value.into_owned())
        .context("ImageFlux の鍵 URI に kid がありません")
}

/// 16 バイトの hex か
fn is_hex_key(key: &str) -> bool {
    key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
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

#[derive(Debug, Deserialize)]
struct EncryptKeyResponse {
    #[serde(default)]
    encrypt_key: String,
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

    #[test]
    fn finds_kid_in_key_uri() {
        let playlist = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://live.example.test/key/uni?metric=100&kid=abc\"\n";
        let key_uri = find_key_uri(playlist).expect("鍵 URI を取得できる");
        assert_eq!(
            key_uri,
            "https://live.example.test/key/uni?metric=100&kid=abc"
        );
        assert_eq!(kid_from_key_uri(&key_uri).unwrap(), "abc");
    }

    #[test]
    fn skips_key_tag_without_uri() {
        let playlist = "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://live.example.test/key/uni?kid=abc\"\n";
        assert_eq!(
            find_key_uri(playlist).unwrap(),
            "https://live.example.test/key/uni?kid=abc"
        );
    }

    #[test]
    fn resolves_variant_urls() {
        let playlist = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nmedia/stream.m3u8\n";
        assert_eq!(
            first_variant_url(playlist, "https://px.example.test/path/master.m3u8").unwrap(),
            "https://px.example.test/path/media/stream.m3u8"
        );

        let playlist =
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nhttps://px.example.test/stream.m3u8\n";
        assert_eq!(
            first_variant_url(playlist, "https://px.example.test/master.m3u8").unwrap(),
            "https://px.example.test/stream.m3u8"
        );

        // タグがなくても m3u8 を指す行をバリアントとして扱う
        let playlist = "#EXTM3U\nchild.m3u8\n";
        assert_eq!(
            first_variant_url(playlist, "https://px.example.test/master.m3u8").unwrap(),
            "https://px.example.test/child.m3u8"
        );
    }

    #[test]
    fn rejects_playlist_without_variant_or_key() {
        let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\nsegment.ts\n";
        assert!(find_key_uri(playlist).is_none());
        assert!(first_variant_url(playlist, "https://px.example.test/live.m3u8").is_err());
    }

    #[test]
    fn validates_encrypt_key() {
        assert!(is_hex_key("0123456789abcdef0123456789abcdef"));
        assert!(is_hex_key("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_hex_key(""));
        assert!(!is_hex_key("0123456789abcdef0123456789abcde"));
        assert!(!is_hex_key("0123456789abcdef0123456789abcdeg"));
    }
}
