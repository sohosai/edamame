//! controller の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::EdamameError;

/// `edamame-controller` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-controller", about = "Edamame controller")]
pub struct ControllerConfig {
    /// HTTP server の待ち受けアドレス
    #[arg(long)]
    pub bind: SocketAddr,

    /// 正規セグメントファイルの保存先
    #[arg(long)]
    pub storage_dir: PathBuf,

    /// メディアプレイリストに含めるセグメント数
    #[arg(long)]
    pub playlist_segments: u64,

    /// worker に指示する sync の間隔
    #[arg(long)]
    pub sync_interval_ms: u64,

    /// worker を DEAD と見なすまでの待ち時間
    #[arg(long)]
    pub sync_timeout_ms: u64,
}

impl ControllerConfig {
    /// 設定値を検証する
    pub fn validate(&self) -> Result<(), EdamameError> {
        if self.playlist_segments < 6 {
            return Err(EdamameError::BadRequest(
                "playlist segments must be at least 6".to_owned(),
            ));
        }
        if self.sync_interval_ms == 0 {
            return Err(EdamameError::BadRequest(
                "sync interval must be positive".to_owned(),
            ));
        }
        if self.sync_timeout_ms == 0 {
            return Err(EdamameError::BadRequest(
                "sync timeout must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> Vec<&'static str> {
        vec![
            "edamame-controller",
            "--bind",
            "127.0.0.1:9000",
            "--storage-dir",
            ".edamame/controller",
            "--playlist-segments",
            "6",
            "--sync-interval-ms",
            "1000",
            "--sync-timeout-ms",
            "10000",
        ]
    }

    #[test]
    fn parses_documented_arguments() {
        let config = ControllerConfig::try_parse_from(base_args()).unwrap();
        assert_eq!(config.playlist_segments, 6);
        assert_eq!(config.sync_interval_ms, 1000);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_short_playlist_segments() {
        let mut args = base_args();
        let index = args
            .iter()
            .position(|value| *value == "--playlist-segments")
            .unwrap();
        args[index + 1] = "5";
        let config = ControllerConfig::try_parse_from(args).unwrap();
        assert!(config.validate().is_err());
    }
}
