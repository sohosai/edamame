//! controller の CLI 設定

use std::net::SocketAddr;

use clap::Parser;
use edamame_domain::{CorsOrigins, EdamameError, normalize_public_url, validate_auth_token};

pub const DEFAULT_PUBLISH_WAIT_MS: u64 = 2000;

/// 容量の指定を byte 数に変換する。`KiB`、`MiB`、`GiB`、`TiB` の接尾辞を付けられる
fn parse_capacity_bytes(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = lower.strip_suffix("kib") {
        (number, 1024_u64)
    } else if let Some(number) = lower.strip_suffix("mib") {
        (number, 1024 * 1024)
    } else if let Some(number) = lower.strip_suffix("gib") {
        (number, 1024 * 1024 * 1024)
    } else if let Some(number) = lower.strip_suffix("tib") {
        (number, 1024 * 1024 * 1024 * 1024)
    } else if lower.bytes().all(|byte| byte.is_ascii_digit()) {
        (lower.as_str(), 1)
    } else {
        return Err("capacity must be bytes or a value with KiB, MiB, GiB, or TiB".to_owned());
    };
    let number = number
        .trim()
        .parse::<u64>()
        .map_err(|_| "capacity must be a number".to_owned())?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "capacity is too large".to_owned())
}

/// `edamame-controller` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-controller", about = "Edamame controller")]
pub struct ControllerConfig {
    /// HTTP サーバーの待ち受けアドレス
    #[arg(long, env = "EDAMAME_BIND")]
    pub bind: SocketAddr,

    /// 正規セグメントファイルをメモリ上に保存する容量。`256MiB` のような 2 進接頭辞付きの値か、byte 数を指定する
    #[arg(
        long,
        env = "EDAMAME_STORAGE_CAPACITY",
        value_parser = parse_capacity_bytes
    )]
    pub storage_capacity: u64,

    /// メディアプレイリストに含めるセグメント数
    #[arg(long, env = "EDAMAME_PLAYLIST_SEGMENTS")]
    pub playlist_segments: u64,

    /// worker に指示する sync の間隔
    #[arg(long, env = "EDAMAME_SYNC_INTERVAL_MS")]
    pub sync_interval_ms: u64,

    /// sync が届かない worker を配信先から外すまでの待ち時間
    #[arg(long, env = "EDAMAME_SYNC_TIMEOUT_MS")]
    pub sync_timeout_ms: u64,

    /// worker が新しいセグメントを取得するための公開待ち
    #[arg(long, env = "EDAMAME_PUBLISH_WAIT_MS")]
    pub publish_wait_ms: Option<u64>,

    /// PUT の認証に使うトークン
    /// 省略すると認証は無効になり、controller の起動時に警告を出す
    #[arg(long, env = "EDAMAME_AUTH_TOKEN", hide_env_values = true)]
    pub auth_token: Option<String>,

    /// ブラウザからの読み取りを許可する origin。`none` で無効、`*` ですべて許可
    #[arg(long, value_delimiter = ',', env = "EDAMAME_CORS_ORIGIN")]
    pub cors_origin: Vec<String>,

    /// peer、worker、client のすべてから到達できる URL
    #[arg(long, env = "EDAMAME_PUBLIC_URL", value_parser = normalize_public_url)]
    pub public_url: Option<String>,

    /// leader の優先度。大きい値を優先する
    #[arg(long, env = "EDAMAME_METRIC")]
    pub metric: Option<u32>,

    /// 最初に接触する controller の URL。カンマ区切りと繰り返しを許可する
    #[arg(long, env = "EDAMAME_SEED", value_delimiter = ',', value_parser = normalize_public_url)]
    pub seed: Vec<String>,

    /// 外部配信へ流す最低の割合（%）
    #[arg(long, env = "EDAMAME_EXTERNAL_MIN_PERCENT", default_value_t = 1)]
    pub external_min_percent: u8,
}

impl ControllerConfig {
    pub fn validate(&self) -> Result<(), EdamameError> {
        if self.storage_capacity == 0 {
            return Err(EdamameError::BadRequest(
                "storage capacity must be positive".to_owned(),
            ));
        }
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
        if self.external_min_percent > 100 {
            return Err(EdamameError::BadRequest(
                "external min percent must be 100 or less".to_owned(),
            ));
        }
        if !self.seed.is_empty() && (self.public_url.is_none() || self.metric.is_none()) {
            return Err(EdamameError::BadRequest(
                "--seed requires --public-url and --metric".to_owned(),
            ));
        }
        if let Some(url) = &self.public_url {
            normalize_public_url(url)?;
        }
        for seed in &self.seed {
            normalize_public_url(seed)?;
        }
        Ok(())
    }

    /// seed を指定しても、共通 token がなければ cluster を有効にしない。
    pub fn cluster_enabled(&self) -> bool {
        !self.seed.is_empty() && self.auth_token.is_some()
    }

    /// 正規化した URL で重複と自分自身を除き、最初の接触先を返す。
    pub fn cluster_seeds(&self) -> Result<Vec<String>, EdamameError> {
        let own = self
            .public_url
            .as_deref()
            .map(normalize_public_url)
            .transpose()?;
        let mut seeds = Vec::new();
        for seed in &self.seed {
            let seed = normalize_public_url(seed)?;
            if own.as_ref() != Some(&seed) && !seeds.contains(&seed) {
                seeds.push(seed);
            }
        }
        Ok(seeds)
    }

    pub fn cors_origins(&self) -> Result<CorsOrigins, EdamameError> {
        CorsOrigins::parse(&self.cors_origin)
    }

    /// 公開待ちが未指定なら既定値を使う
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

    /// 値の検証用の設定
    /// CLI 引数と環境変数の解析は、controller を起動するテストで確認する
    fn config() -> ControllerConfig {
        ControllerConfig {
            bind: "127.0.0.1:9000".parse().unwrap(),
            storage_capacity: 16 * 1024 * 1024,
            playlist_segments: 6,
            sync_interval_ms: 1000,
            sync_timeout_ms: 10000,
            publish_wait_ms: None,
            auth_token: None,
            cors_origin: Vec::new(),
            external_min_percent: 1,
            public_url: None,
            metric: None,
            seed: Vec::new(),
        }
    }

    #[test]
    fn accepts_valid_values() {
        config().validate().unwrap();
    }

    #[test]
    fn rejects_zero_storage_capacity() {
        let mut config = config();
        config.storage_capacity = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_storage_capacity_with_binary_prefixes() {
        assert_eq!(parse_capacity_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_capacity_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_capacity_bytes("256MiB").unwrap(), 268_435_456);
        assert_eq!(parse_capacity_bytes("1GiB").unwrap(), 1_073_741_824);
        assert_eq!(parse_capacity_bytes("1TiB").unwrap(), 1_099_511_627_776);
        // 大文字と小文字のどちらでも受け付ける
        assert_eq!(parse_capacity_bytes("512mib").unwrap(), 536_870_912);
    }

    #[test]
    fn rejects_invalid_storage_capacity() {
        assert!(parse_capacity_bytes("").is_err());
        assert!(parse_capacity_bytes("256MB").is_err());
        assert!(parse_capacity_bytes("MiB").is_err());
        assert!(parse_capacity_bytes("-1MiB").is_err());
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
    fn accepts_external_min_percent_range() {
        let mut config = config();
        for value in [0, 1, 100] {
            config.external_min_percent = value;
            config.validate().unwrap();
        }
        config.external_min_percent = 101;
        assert!(config.validate().is_err());
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
    fn validates_cluster_option_combinations() {
        for public_url in [None, Some("https://a.test/base".to_owned())] {
            for metric in [None, Some(0), Some(u32::MAX)] {
                for token in [None, Some("token".to_owned())] {
                    let mut config = config();
                    config.public_url = public_url.clone();
                    config.metric = metric;
                    config.auth_token = token;
                    config.validate().unwrap();
                    assert!(!config.cluster_enabled());
                    config.seed = vec!["http://b.test".to_owned()];
                    // token の有無にかかわらず、seed を指定したら URL と metric を揃える。
                    assert_eq!(
                        config.validate().is_ok(),
                        public_url.is_some() && metric.is_some()
                    );
                    if config.validate().is_ok() {
                        assert_eq!(config.cluster_enabled(), config.auth_token.is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn normalizes_cluster_urls_and_deduplicates_seeds() {
        let mut config = config();
        config.public_url = Some("https://A.test/base/".to_owned());
        config.seed = vec![
            "https://a.test/base".to_owned(),
            "https://B.test/".to_owned(),
            "https://b.test".to_owned(),
            "http://c.test/path/".to_owned(),
        ];
        assert_eq!(
            config.cluster_seeds().unwrap(),
            vec!["https://b.test", "http://c.test/path"]
        );
    }

    #[test]
    fn rejects_invalid_cluster_urls_and_metric() {
        let base = [
            "controller",
            "--bind",
            "127.0.0.1:0",
            "--storage-capacity",
            "1MiB",
            "--playlist-segments",
            "6",
            "--sync-interval-ms",
            "100",
            "--sync-timeout-ms",
            "1000",
        ];
        for option in ["--public-url", "--seed"] {
            for url in [
                "/relative",
                "ftp://a.test",
                "http://a.test/?q=1",
                "http://a.test/#fragment",
            ] {
                assert!(
                    ControllerConfig::try_parse_from(base.into_iter().chain([option, url]))
                        .is_err()
                );
            }
        }
        assert!(
            ControllerConfig::try_parse_from(base.into_iter().chain(["--metric", "4294967296"]))
                .is_err()
        );
        let config = ControllerConfig::try_parse_from(base.into_iter().chain([
            "--public-url",
            "https://A.test/base/",
            "--metric",
            "0",
            "--seed",
            "https://b.test,http://c.test",
            "--seed",
            "http://d.test/",
        ]))
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.public_url.as_deref(), Some("https://a.test/base"));
        assert_eq!(config.cluster_seeds().unwrap().len(), 3);
    }

    #[test]
    fn names_environment_variables() {
        // 環境変数を持つ option について、環境変数名が `EDAMAME_` と option 名に対応することを確かめる
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
        assert_eq!(names.len(), 12);
        assert_eq!(names["public_url"], "EDAMAME_PUBLIC_URL");
        assert_eq!(names["metric"], "EDAMAME_METRIC");
        assert_eq!(names["seed"], "EDAMAME_SEED");
        assert_eq!(names["bind"], "EDAMAME_BIND");
        assert_eq!(names["storage_capacity"], "EDAMAME_STORAGE_CAPACITY");
        assert_eq!(names["playlist_segments"], "EDAMAME_PLAYLIST_SEGMENTS");
        assert_eq!(names["sync_interval_ms"], "EDAMAME_SYNC_INTERVAL_MS");
        assert_eq!(names["sync_timeout_ms"], "EDAMAME_SYNC_TIMEOUT_MS");
        assert_eq!(names["publish_wait_ms"], "EDAMAME_PUBLISH_WAIT_MS");
        assert_eq!(names["auth_token"], "EDAMAME_AUTH_TOKEN");
        assert_eq!(names["cors_origin"], "EDAMAME_CORS_ORIGIN");
        assert_eq!(
            names["external_min_percent"],
            "EDAMAME_EXTERNAL_MIN_PERCENT"
        );
    }
}
