//! controller の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError};

pub const DEFAULT_PUBLISH_WAIT_MS: u64 = 2000;

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

    /// sync が届かない worker を配信先から外すまでの待ち時間
    #[arg(long)]
    pub sync_timeout_ms: u64,

    /// 新しいセグメントを公開する前に worker の取得を待つ時間
    #[arg(long)]
    pub publish_wait_ms: Option<u64>,

    /// ブラウザからの読み取りを許可する origin。`none` で無効、`*` ですべて許可
    #[arg(long, value_delimiter = ',')]
    pub cors_origin: Vec<String>,
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

    /// CORS の設定を検証して返す
    pub fn cors_origins(&self) -> Result<CorsOrigins, EdamameError> {
        CorsOrigins::parse(&self.cors_origin)
    }

    /// 公開までの待ち時間を返す。未指定なら既定値を使う。
    #[must_use]
    pub fn publish_wait_ms(&self) -> u64 {
        self.publish_wait_ms.unwrap_or(DEFAULT_PUBLISH_WAIT_MS)
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

    #[test]
    fn parses_cors_origins() {
        let mut args = base_args();
        args.extend([
            "--cors-origin",
            "http://a.test,http://b.test",
            "--cors-origin",
            "http://a.test",
        ]);
        let config = ControllerConfig::try_parse_from(args).unwrap();
        assert_eq!(config.cors_origin.len(), 3);
        assert_eq!(
            config.cors_origins().unwrap(),
            CorsOrigins::List(vec!["http://a.test".to_owned(), "http://b.test".to_owned()])
        );
    }

    #[test]
    fn treats_missing_cors_origin_as_disabled() {
        let config = ControllerConfig::try_parse_from(base_args()).unwrap();
        assert!(config.cors_origin.is_empty());
        assert_eq!(config.cors_origins().unwrap(), CorsOrigins::Disabled);
    }

    #[test]
    fn uses_default_publish_wait_when_missing() {
        let config = ControllerConfig::try_parse_from(base_args()).unwrap();
        assert_eq!(config.publish_wait_ms, None);
        assert_eq!(config.publish_wait_ms(), DEFAULT_PUBLISH_WAIT_MS);
    }

    #[test]
    fn accepts_explicit_publish_wait() {
        let mut args = base_args();
        args.extend(["--publish-wait-ms", "0"]);
        let config = ControllerConfig::try_parse_from(args).unwrap();
        assert_eq!(config.publish_wait_ms(), 0);
    }

    #[test]
    fn rejects_cors_origin_combined_with_none() {
        let mut args = base_args();
        args.extend(["--cors-origin", "none", "--cors-origin", "http://a.test"]);
        let config = ControllerConfig::try_parse_from(args).unwrap();
        assert!(config.cors_origins().is_err());
    }
}
