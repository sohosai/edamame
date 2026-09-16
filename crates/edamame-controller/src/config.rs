//! controller の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError, validate_auth_token};

pub const DEFAULT_PUBLISH_WAIT_MS: u64 = 2000;

/// `edamame-controller` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-controller", about = "Edamame controller")]
pub struct ControllerConfig {
    /// HTTP server の待ち受けアドレス
    #[arg(long, env = "EDAMAME_BIND")]
    pub bind: SocketAddr,

    /// 正規セグメントファイルの保存先
    #[arg(long, env = "EDAMAME_STORAGE_DIR")]
    pub storage_dir: PathBuf,

    /// メディアプレイリストに含めるセグメント数
    #[arg(long, env = "EDAMAME_PLAYLIST_SEGMENTS")]
    pub playlist_segments: u64,

    /// worker に指示する sync の間隔
    #[arg(long, env = "EDAMAME_SYNC_INTERVAL_MS")]
    pub sync_interval_ms: u64,

    /// sync が届かない worker を配信先から外すまでの待ち時間
    #[arg(long, env = "EDAMAME_SYNC_TIMEOUT_MS")]
    pub sync_timeout_ms: u64,

    /// 新しいセグメントを公開する前に worker の取得を待つ時間
    #[arg(long, env = "EDAMAME_PUBLISH_WAIT_MS")]
    pub publish_wait_ms: Option<u64>,

    /// PUT の認証に使うトークン。省略すると PUT の認証を無効にし、起動時に警告を出す
    #[arg(long, env = "EDAMAME_AUTH_TOKEN", hide_env_values = true)]
    pub auth_token: Option<String>,

    /// ブラウザからの読み取りを許可する origin。`none` で無効、`*` ですべて許可
    #[arg(long, value_delimiter = ',', env = "EDAMAME_CORS_ORIGIN")]
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
        if let Some(token) = &self.auth_token {
            validate_auth_token(token)?;
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
    use std::collections::BTreeMap;

    use super::*;
    use clap::CommandFactory;

    /// テストで使う設定。option の解析は実バイナリのテストで確認する
    fn config() -> ControllerConfig {
        ControllerConfig {
            bind: "127.0.0.1:9000".parse().unwrap(),
            storage_dir: PathBuf::from(".edamame/controller"),
            playlist_segments: 6,
            sync_interval_ms: 1000,
            sync_timeout_ms: 10000,
            publish_wait_ms: None,
            auth_token: None,
            cors_origin: Vec::new(),
        }
    }

    #[test]
    fn accepts_valid_values() {
        config().validate().unwrap();
    }

    #[test]
    fn rejects_short_playlist_segments() {
        let mut config = config();
        config.playlist_segments = 5;
        assert!(config.validate().is_err());
    }

    #[test]
    fn normalizes_and_deduplicates_cors_origins() {
        let mut config = config();
        config.cors_origin = vec![
            "http://a.test".to_owned(),
            "http://b.test".to_owned(),
            "http://a.test".to_owned(),
        ];
        assert_eq!(
            config.cors_origins().unwrap(),
            CorsOrigins::List(vec!["http://a.test".to_owned(), "http://b.test".to_owned()])
        );
    }

    #[test]
    fn treats_missing_cors_origin_as_disabled() {
        assert_eq!(config().cors_origins().unwrap(), CorsOrigins::Disabled);
    }

    #[test]
    fn uses_default_publish_wait_when_missing() {
        assert_eq!(config().publish_wait_ms(), DEFAULT_PUBLISH_WAIT_MS);
    }

    #[test]
    fn accepts_explicit_publish_wait() {
        let mut config = config();
        config.publish_wait_ms = Some(0);
        assert_eq!(config.publish_wait_ms(), 0);
    }

    #[test]
    fn treats_missing_auth_token_as_disabled() {
        let mut config = config();
        config.auth_token = None;
        config.validate().unwrap();
    }

    #[test]
    fn accepts_auth_token() {
        let mut config = config();
        config.auth_token = Some("example-token".to_owned());
        config.validate().unwrap();
    }

    #[test]
    fn rejects_empty_auth_token() {
        let mut config = config();
        config.auth_token = Some(String::new());
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_cors_origin_combined_with_none() {
        let mut config = config();
        config.cors_origin = vec!["none".to_owned(), "http://a.test".to_owned()];
        assert!(config.cors_origins().is_err());
    }

    #[test]
    fn splits_cors_origins_with_comma() {
        // コマンドライン引数と環境変数のどちらでもカンマ区切りを受け付ける
        let delimiter = ControllerConfig::command()
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == "cors_origin")
            .and_then(|arg| arg.get_value_delimiter());
        assert_eq!(delimiter, Some(','));
    }

    #[test]
    fn names_environment_variables() {
        // 全 option に `EDAMAME_` と option 名からなる環境変数名が設定されている
        let names = ControllerConfig::command()
            .get_arguments()
            .filter_map(|arg| {
                let name = arg.get_env()?;
                Some((
                    arg.get_id().as_str().to_owned(),
                    name.to_string_lossy().into_owned(),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(names.len(), 8);
        assert_eq!(names["bind"], "EDAMAME_BIND");
        assert_eq!(names["storage_dir"], "EDAMAME_STORAGE_DIR");
        assert_eq!(names["playlist_segments"], "EDAMAME_PLAYLIST_SEGMENTS");
        assert_eq!(names["sync_interval_ms"], "EDAMAME_SYNC_INTERVAL_MS");
        assert_eq!(names["sync_timeout_ms"], "EDAMAME_SYNC_TIMEOUT_MS");
        assert_eq!(names["publish_wait_ms"], "EDAMAME_PUBLISH_WAIT_MS");
        assert_eq!(names["auth_token"], "EDAMAME_AUTH_TOKEN");
        assert_eq!(names["cors_origin"], "EDAMAME_CORS_ORIGIN");
    }
}
