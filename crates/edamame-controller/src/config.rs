//! controller の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError};

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
    fn rejects_cors_origin_combined_with_none() {
        let mut args = base_args();
        args.extend(["--cors-origin", "none", "--cors-origin", "http://a.test"]);
        let config = ControllerConfig::try_parse_from(args).unwrap();
        assert!(config.cors_origins().is_err());
    }
}
