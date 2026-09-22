//! worker の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError, validate_auth_token, validate_identifier};

/// ACME ディレクトリの既定値（Let's Encrypt の本番環境）
const DEFAULT_ACME_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// client への配信で使う TLS の設定
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// TLS を使わない
    Plain,
    /// PEM ファイルの証明書で TLS 配信する
    Static { cert: PathBuf, key: PathBuf },
    /// ACME で証明書を取得して TLS 配信する
    Acme {
        domain: String,
        directory: String,
        cache_dir: PathBuf,
        contact: Option<String>,
    },
}

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

    /// client に公開する worker の URL（http または https）
    #[arg(long, env = "EDAMAME_PUBLIC_URL")]
    pub public_url: String,

    /// controller の接続先。http の絶対 URL のみ許可する
    #[arg(long, env = "EDAMAME_CONTROLLER_URL")]
    pub controller_url: String,

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
        self.tls_mode()?;
        Ok(())
    }

    /// TLS の設定を解決する
    pub fn tls_mode(&self) -> Result<TlsMode, EdamameError> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                if self.acme {
                    return Err(EdamameError::BadRequest(
                        "--acme cannot be combined with --tls-cert".to_owned(),
                    ));
                }
                require_https_public_url(&self.public_url, "TLS")?;
                Ok(TlsMode::Static {
                    cert: cert.clone(),
                    key: key.clone(),
                })
            }
            (None, None) if self.acme => {
                let cache_dir = self.acme_cache_dir.clone().ok_or_else(|| {
                    EdamameError::BadRequest("--acme-cache-dir is required with --acme".to_owned())
                })?;
                let url = require_https_public_url(&self.public_url, "ACME")?;
                let domain = validate_acme_domain(url.host_str().ok_or_else(|| {
                    EdamameError::BadRequest("--public-url has no host".to_owned())
                })?)?;
                let directory = match &self.acme_directory {
                    Some(directory) => validate_acme_directory(directory)?,
                    None => DEFAULT_ACME_DIRECTORY.to_owned(),
                };
                let contact = self
                    .acme_contact
                    .as_deref()
                    .map(validate_acme_contact)
                    .transpose()?;
                Ok(TlsMode::Acme {
                    domain,
                    directory,
                    cache_dir,
                    contact,
                })
            }
            (None, None) => Ok(TlsMode::Plain),
            _ => Err(EdamameError::BadRequest(
                "--tls-cert and --tls-key must be specified together".to_owned(),
            )),
        }
    }

    /// CORS の設定を検証して返す
    pub fn cors_origins(&self) -> Result<CorsOrigins, EdamameError> {
        CorsOrigins::parse(&self.cors_origin)
    }
}

/// TLS 配信で使うため、`--public-url` が https のときだけ URL を返す
fn require_https_public_url(value: &str, purpose: &str) -> Result<url::Url, EdamameError> {
    let url = url::Url::parse(value.trim())
        .map_err(|error| EdamameError::BadRequest(format!("public_url is invalid: {error}")))?;
    if url.scheme() != "https" {
        return Err(EdamameError::BadRequest(format!(
            "public_url must be https for {purpose}"
        )));
    }
    Ok(url)
}

/// ACME で証明書を取得する DNS 名を検証する
fn validate_acme_domain(value: &str) -> Result<String, EdamameError> {
    let domain = value.trim();
    if domain.is_empty() || domain.len() > 253 {
        return Err(EdamameError::BadRequest(
            "acme domain is invalid".to_owned(),
        ));
    }
    if domain.parse::<std::net::IpAddr>().is_ok() {
        return Err(EdamameError::BadRequest(
            "acme domain must be a DNS name, not an IP address".to_owned(),
        ));
    }
    if !domain.contains('.') {
        return Err(EdamameError::BadRequest(
            "acme domain must contain a dot".to_owned(),
        ));
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(EdamameError::BadRequest(
                "acme domain is invalid".to_owned(),
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(EdamameError::BadRequest(
                "acme domain is invalid".to_owned(),
            ));
        }
        if !label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(EdamameError::BadRequest(
                "acme domain is invalid".to_owned(),
            ));
        }
    }
    Ok(domain.to_ascii_lowercase())
}

/// ACME ディレクトリが https の絶対 URL であることを検証する
fn validate_acme_directory(value: &str) -> Result<String, EdamameError> {
    let url = url::Url::parse(value.trim())
        .map_err(|error| EdamameError::BadRequest(format!("acme directory is invalid: {error}")))?;
    if url.scheme() != "https" || url.host_str().is_none() || url.fragment().is_some() {
        return Err(EdamameError::BadRequest(
            "acme directory must be an https URL without a fragment".to_owned(),
        ));
    }
    Ok(url.to_string())
}

/// ACME の連絡先メールを検証する
fn validate_acme_contact(value: &str) -> Result<String, EdamameError> {
    let contact = value.trim();
    let Some((local, domain)) = contact.split_once('@') else {
        return Err(EdamameError::BadRequest(
            "acme contact must be an email address".to_owned(),
        ));
    };
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || contact
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(EdamameError::BadRequest(
            "acme contact must be an email address".to_owned(),
        ));
    }
    Ok(contact.to_owned())
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
            controller_url: "http://127.0.0.1:9000".to_owned(),
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
    fn uses_plain_tls_by_default() {
        assert_eq!(config().tls_mode().unwrap(), TlsMode::Plain);
    }

    #[test]
    fn accepts_static_tls_with_https_public_url() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.tls_cert = Some(PathBuf::from("/certs/fullchain.pem"));
        config.tls_key = Some(PathBuf::from("/certs/privkey.pem"));
        assert_eq!(
            config.tls_mode().unwrap(),
            TlsMode::Static {
                cert: PathBuf::from("/certs/fullchain.pem"),
                key: PathBuf::from("/certs/privkey.pem"),
            }
        );
    }

    #[test]
    fn rejects_static_tls_with_http_public_url() {
        let mut config = config();
        config.tls_cert = Some(PathBuf::from("/certs/fullchain.pem"));
        config.tls_key = Some(PathBuf::from("/certs/privkey.pem"));
        assert!(config.tls_mode().is_err());
    }

    #[test]
    fn rejects_tls_cert_without_key() {
        let mut config = config();
        config.tls_cert = Some(PathBuf::from("/certs/fullchain.pem"));
        assert!(config.tls_mode().is_err());
    }

    #[test]
    fn rejects_acme_with_static_cert() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.tls_cert = Some(PathBuf::from("/certs/fullchain.pem"));
        config.tls_key = Some(PathBuf::from("/certs/privkey.pem"));
        config.acme = true;
        assert!(config.tls_mode().is_err());
    }

    #[test]
    fn uses_public_url_host_and_lets_encrypt_by_default() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.acme = true;
        config.acme_cache_dir = Some(PathBuf::from("/acme"));
        assert_eq!(
            config.tls_mode().unwrap(),
            TlsMode::Acme {
                domain: "worker.test".to_owned(),
                directory: "https://acme-v02.api.letsencrypt.org/directory".to_owned(),
                cache_dir: PathBuf::from("/acme"),
                contact: None,
            }
        );
    }

    #[test]
    fn accepts_explicit_acme_directory() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.acme = true;
        config.acme_cache_dir = Some(PathBuf::from("/acme"));
        config.acme_directory =
            Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_owned());
        config.acme_contact = Some("ops@example.test".to_owned());
        assert_eq!(
            config.tls_mode().unwrap(),
            TlsMode::Acme {
                domain: "worker.test".to_owned(),
                directory: "https://acme-staging-v02.api.letsencrypt.org/directory".to_owned(),
                cache_dir: PathBuf::from("/acme"),
                contact: Some("ops@example.test".to_owned()),
            }
        );
    }

    #[test]
    fn rejects_acme_without_cache_dir() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.acme = true;
        assert!(config.tls_mode().is_err());
    }

    #[test]
    fn rejects_acme_with_ip_domain() {
        let mut config = config();
        config.public_url = "https://127.0.0.1:9001".to_owned();
        config.acme = true;
        config.acme_cache_dir = Some(PathBuf::from("/acme"));
        assert!(config.tls_mode().is_err());
    }

    #[test]
    fn rejects_invalid_acme_directory_and_contact() {
        let mut config = config();
        config.public_url = "https://worker.test".to_owned();
        config.acme = true;
        config.acme_cache_dir = Some(PathBuf::from("/acme"));
        config.acme_directory = Some("ftp://worker.test/directory".to_owned());
        assert!(config.tls_mode().is_err());
        config.acme_directory = None;
        config.acme_contact = Some("not-an-email".to_owned());
        assert!(config.tls_mode().is_err());
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
