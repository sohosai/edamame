//! worker の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{
    CorsOrigins, EdamameError, normalize_public_url, validate_auth_token, validate_identifier,
};

pub use edamame_tls::TlsMode;

/// `edamame-worker` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-worker", about = "Edamame worker")]
pub struct WorkerConfig {
    /// controller に登録する worker の識別子
    #[arg(long, env = "EDAMAME_WORKER_ID")]
    pub worker_id: String,

    /// HTTP または TLS サーバーの待ち受けアドレス
    #[arg(long, env = "EDAMAME_BIND")]
    pub bind: SocketAddr,

    /// client に公開する worker の URL（http または https）
    #[arg(long, env = "EDAMAME_PUBLIC_URL")]
    pub public_url: String,

    /// controller の接続先。http または https の絶対 URL を指定する。
    /// カンマ区切りと繰り返しを許可し、接続に失敗したら次の URL へ切り替える
    #[arg(
        long,
        env = "EDAMAME_CONTROLLER_URL",
        value_delimiter = ',',
        required = true,
        value_parser = normalize_public_url
    )]
    pub controller_url: Vec<String>,

    /// (channel_id, quality) ごとのキャッシュ上限
    #[arg(long, env = "EDAMAME_CACHE_LIMIT")]
    pub cache_limit: u64,

    /// client に配信する TLS 証明書の PEM ファイル。`--tls-key` と一緒に指定する
    #[arg(
        long,
        env = "EDAMAME_TLS_CERT",
        requires = "tls_key",
        conflicts_with = "acme"
    )]
    pub tls_cert: Option<PathBuf>,

    /// TLS 証明書の秘密鍵の PEM ファイル。`--tls-cert` と一緒に指定する
    #[arg(
        long,
        env = "EDAMAME_TLS_KEY",
        requires = "tls_cert",
        conflicts_with = "acme"
    )]
    pub tls_key: Option<PathBuf>,

    /// ACME で証明書を取得して TLS 配信する
    #[arg(long, env = "EDAMAME_ACME", conflicts_with_all = ["tls_cert", "tls_key"])]
    pub acme: bool,

    /// ACME ディレクトリの URL。省略時は Let's Encrypt の本番環境
    #[arg(long, env = "EDAMAME_ACME_DIRECTORY", requires = "acme")]
    pub acme_directory: Option<String>,

    /// ACME のアカウント鍵と証明書の保存先。`--acme` と一緒に必須
    #[arg(long, env = "EDAMAME_ACME_CACHE_DIR", requires = "acme")]
    pub acme_cache_dir: Option<PathBuf>,

    /// ACME の連絡先メール。省略すると起動時に警告を出す
    #[arg(long, env = "EDAMAME_ACME_CONTACT", requires = "acme")]
    pub acme_contact: Option<String>,

    /// worker の重み（相対的な配信能力）
    #[arg(long, env = "EDAMAME_WEIGHT", default_value_t = 1)]
    pub weight: u32,

    /// controller への登録 PUT にだけ付ける認証トークン
    /// 省略すると Authorization ヘッダーを送らない
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
        if self.weight == 0 {
            return Err(EdamameError::BadRequest(
                "weight must be positive".to_owned(),
            ));
        }
        if self.controller_url.is_empty() {
            return Err(EdamameError::BadRequest(
                "controller URL is required".to_owned(),
            ));
        }
        self.tls_mode()?;
        Ok(())
    }

    /// TLS の設定を解決する
    pub fn tls_mode(&self) -> Result<TlsMode, EdamameError> {
        edamame_tls::resolve_tls_mode(
            &self.public_url,
            self.tls_cert.as_deref(),
            self.tls_key.as_deref(),
            self.acme,
            self.acme_directory.as_deref(),
            self.acme_cache_dir.as_deref(),
            self.acme_contact.as_deref(),
        )
    }

    /// CORS の設定を検証して返す
    pub fn cors_origins(&self) -> Result<CorsOrigins, EdamameError> {
        CorsOrigins::parse(&self.cors_origin)
    }

    /// 重複を除いた controller の URL を、指定した順に返す
    #[must_use]
    pub fn controller_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        for url in &self.controller_url {
            if !urls.contains(url) {
                urls.push(url.clone());
            }
        }
        urls
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use clap::CommandFactory;

    /// CLI の option の解析は、バイナリを起動するテストで確認する
    fn config() -> WorkerConfig {
        WorkerConfig {
            worker_id: "worker-a".to_owned(),
            bind: "127.0.0.1:9001".parse().unwrap(),
            public_url: "http://127.0.0.1:9001".to_owned(),
            controller_url: vec!["http://127.0.0.1:9000".to_owned()],
            cache_limit: 12,
            tls_cert: None,
            tls_key: None,
            acme: false,
            acme_directory: None,
            acme_cache_dir: None,
            acme_contact: None,
            weight: 1,
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
    fn rejects_zero_weight() {
        let mut config = config();
        config.weight = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_empty_controller_url() {
        let mut config = config();
        config.controller_url = Vec::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn deduplicates_controller_urls_in_order() {
        let mut config = config();
        config.controller_url = vec![
            "https://b.test".to_owned(),
            "https://a.test".to_owned(),
            "https://b.test".to_owned(),
        ];
        assert_eq!(
            config.controller_urls(),
            vec!["https://b.test".to_owned(), "https://a.test".to_owned()]
        );
    }

    #[test]
    fn parses_multiple_controller_urls() {
        // カンマ区切りと繰り返しを受け付け、末尾の `/` を取り除く
        let config = WorkerConfig::try_parse_from([
            "edamame-worker",
            "--worker-id",
            "worker-a",
            "--bind",
            "127.0.0.1:9001",
            "--public-url",
            "http://127.0.0.1:9001",
            "--cache-limit",
            "12",
            "--controller-url",
            "https://a.test/,https://b.test",
            "--controller-url",
            "https://c.test",
        ])
        .unwrap();
        assert_eq!(
            config.controller_url,
            vec![
                "https://a.test".to_owned(),
                "https://b.test".to_owned(),
                "https://c.test".to_owned(),
            ]
        );
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
        assert_eq!(names.len(), 14);
        assert_eq!(names["worker_id"], "EDAMAME_WORKER_ID");
        assert_eq!(names["bind"], "EDAMAME_BIND");
        assert_eq!(names["public_url"], "EDAMAME_PUBLIC_URL");
        assert_eq!(names["controller_url"], "EDAMAME_CONTROLLER_URL");
        assert_eq!(names["cache_limit"], "EDAMAME_CACHE_LIMIT");
        assert_eq!(names["tls_cert"], "EDAMAME_TLS_CERT");
        assert_eq!(names["tls_key"], "EDAMAME_TLS_KEY");
        assert_eq!(names["acme"], "EDAMAME_ACME");
        assert_eq!(names["acme_directory"], "EDAMAME_ACME_DIRECTORY");
        assert_eq!(names["acme_cache_dir"], "EDAMAME_ACME_CACHE_DIR");
        assert_eq!(names["acme_contact"], "EDAMAME_ACME_CONTACT");
        assert_eq!(names["weight"], "EDAMAME_WEIGHT");
        assert_eq!(names["auth_token"], "EDAMAME_AUTH_TOKEN");
        assert_eq!(names["cors_origin"], "EDAMAME_CORS_ORIGIN");
    }
}
