//! OBS WebSocket 5 の制御

use anyhow::{Context, Result, bail};
use obws::Client;
use serde_json::Value;

use crate::config::ObsConfig;

/// OBS の配信状態
#[derive(Debug, Clone, Default)]
pub struct StreamSnapshot {
    pub active: bool,
    pub reconnecting: bool,
    pub timecode_seconds: i64,
    pub duration_seconds: i64,
    pub bytes: u64,
    pub skipped_frames: u32,
    pub total_frames: u32,
}

/// 変更前の配信先
#[derive(Debug, Clone)]
pub struct PreviousStream {
    service_type: String,
    settings: Value,
}

/// OBS の制御クライアント
pub struct ObsClient {
    client: Client,
}

impl ObsClient {
    /// OBS に接続する
    pub async fn connect(config: &ObsConfig) -> Result<Self> {
        let password = if config.password.is_empty() {
            None
        } else {
            Some(config.password.as_str())
        };
        let client = Client::connect(&config.host, config.port, password)
            .await
            .with_context(|| {
                format!(
                    "OBS WebSocket に接続できません ({}:{})",
                    config.host, config.port
                )
            })?;
        Ok(Self { client })
    }

    /// 現在の配信状態
    pub async fn status(&self) -> Result<StreamSnapshot> {
        let status = self
            .client
            .streaming()
            .status()
            .await
            .context("OBS の配信状態を取得できません")?;
        Ok(StreamSnapshot {
            active: status.active,
            reconnecting: status.reconnecting,
            timecode_seconds: status.timecode.whole_seconds(),
            duration_seconds: status.duration.whole_seconds(),
            bytes: status.bytes,
            skipped_frames: status.skipped_frames,
            total_frames: status.total_frames,
        })
    }

    /// 既に配信中なら開始しない
    pub async fn ensure_idle(&self) -> Result<()> {
        let status = self.status().await?;
        if status.active {
            bail!("OBS が既に配信中です。OBS 側で停止してから開始してください");
        }
        Ok(())
    }

    /// 変更前の配信先を保存する
    pub async fn previous_stream(&self) -> Result<PreviousStream> {
        let current = self
            .client
            .config()
            .stream_service_settings::<Value>()
            .await
            .context("OBS の配信先を取得できません")?;
        Ok(PreviousStream {
            service_type: current.r#type,
            settings: current.settings,
        })
    }

    /// ローカル RTMP を配信先に設定する
    pub async fn set_local_rtmp(&self, server: &str, key: &str) -> Result<()> {
        let settings = serde_json::json!({
            "server": server,
            "key": key,
        });
        self.client
            .config()
            .set_stream_service_settings("rtmp_custom", &settings)
            .await
            .context("OBS の配信先を変更できません")
    }

    /// 保存した配信先へ戻す
    pub async fn restore(&self, previous: &PreviousStream) -> Result<()> {
        self.client
            .config()
            .set_stream_service_settings(&previous.service_type, &previous.settings)
            .await
            .context("OBS の配信先を元へ戻せません")
    }

    /// 配信を開始する
    pub async fn start(&self) -> Result<()> {
        self.client
            .streaming()
            .start()
            .await
            .context("OBS の配信を開始できません")
    }

    /// 配信を停止する
    pub async fn stop(&self) -> Result<()> {
        self.client
            .streaming()
            .stop()
            .await
            .context("OBS の配信を停止できません")
    }

    /// 現在の配信先が指定したローカル RTMP か
    pub async fn is_local_rtmp(&self, server: &str, key: &str) -> bool {
        let Ok(current) = self
            .client
            .config()
            .stream_service_settings::<Value>()
            .await
        else {
            return false;
        };
        current.r#type == "rtmp_custom"
            && current.settings.get("server").and_then(Value::as_str) == Some(server)
            && current.settings.get("key").and_then(Value::as_str) == Some(key)
    }
}
