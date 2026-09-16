//! worker の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError, validate_auth_token, validate_identifier};

/// `edamame-worker` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-worker", about = "Edamame worker")]
pub struct WorkerConfig {
    /// controller に登録する worker の識別子
    #[arg(long, env = "EDAMAME_WORKER_ID")]
    pub worker_id: String,

    /// HTTP server の待ち受けアドレス
    #[arg(long, env = "EDAMAME_BIND")]
    pub bind: SocketAddr,

    /// client に公開する worker の URL
    #[arg(long, env = "EDAMAME_PUBLIC_URL")]
    pub public_url: String,

    /// controller に接続するための URL
    #[arg(long, env = "EDAMAME_CONTROLLER_URL")]
    pub controller_url: String,

    /// セグメントファイルのキャッシュ先
    #[arg(long, env = "EDAMAME_CACHE_DIR")]
    pub cache_dir: PathBuf,

    /// (channel_id, quality) ごとのキャッシュ上限
    #[arg(long, env = "EDAMAME_CACHE_LIMIT")]
    pub cache_limit: u64,

    /// controller の認証に使うトークン。省略すると Authorization ヘッダーを送らない
    #[arg(long, env = "EDAMAME_AUTH_TOKEN", hide_env_values = true)]
    pub auth_token: Option<String>,

    /// ブラウザからの読み取りを許可する origin。`none` で無効、`*` ですべて許可
    #[arg(long, value_delimiter = ',', env = "EDAMAME_CORS_ORIGIN")]
    pub cors_origin: Vec<String>,
}

impl WorkerConfig {
    /// 設定値を検証する
    pub fn validate(&self) -> Result<(), EdamameError> {
        validate_identifier("worker_id", &self.worker_id)?;
        if let Some(token) = &self.auth_token {
            validate_auth_token(token)?;
        }
        if self.cache_limit == 0 {
            return Err(EdamameError::BadRequest(
                "cache limit must be positive".to_owned(),
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
    use std::collections::BTreeMap;

    use super::*;
    use clap::CommandFactory;

    /// テストで使う設定。option の解析は実バイナリのテストで確認する
    fn config() -> WorkerConfig {
        WorkerConfig {
            worker_id: "worker-a".to_owned(),
            bind: "127.0.0.1:9001".parse().unwrap(),
            public_url: "http://127.0.0.1:9001".to_owned(),
            controller_url: "http://127.0.0.1:9000".to_owned(),
            cache_dir: PathBuf::from(".edamame/worker-a"),
            cache_limit: 12,
            auth_token: None,
            cors_origin: Vec::new(),
        }
    }

    #[test]
    fn accepts_valid_values() {
        config().validate().unwrap();
    }

    #[test]
    fn rejects_zero_cache_limit() {
        let mut config = config();
        config.cache_limit = 0;
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
    fn splits_cors_origins_with_comma() {
        // コマンドライン引数と環境変数のどちらでもカンマ区切りを受け付ける
        let delimiter = WorkerConfig::command()
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == "cors_origin")
            .and_then(|arg| arg.get_value_delimiter());
        assert_eq!(delimiter, Some(','));
    }

    #[test]
    fn names_environment_variables() {
        // 全 option に `EDAMAME_` と option 名からなる環境変数名が設定されている
        let names = WorkerConfig::command()
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
        assert_eq!(names["worker_id"], "EDAMAME_WORKER_ID");
        assert_eq!(names["bind"], "EDAMAME_BIND");
        assert_eq!(names["public_url"], "EDAMAME_PUBLIC_URL");
        assert_eq!(names["controller_url"], "EDAMAME_CONTROLLER_URL");
        assert_eq!(names["cache_dir"], "EDAMAME_CACHE_DIR");
        assert_eq!(names["cache_limit"], "EDAMAME_CACHE_LIMIT");
        assert_eq!(names["auth_token"], "EDAMAME_AUTH_TOKEN");
        assert_eq!(names["cors_origin"], "EDAMAME_CORS_ORIGIN");
    }
}
