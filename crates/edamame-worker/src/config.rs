//! worker の CLI 設定

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use edamame_domain::{EdamameError, validate_identifier};

/// `edamame-worker` の設定
#[derive(Debug, Clone, Parser)]
#[command(name = "edamame-worker", about = "Edamame worker")]
pub struct WorkerConfig {
    /// controller に登録する worker の識別子
    #[arg(long)]
    pub worker_id: String,

    /// HTTP server の待ち受けアドレス
    #[arg(long)]
    pub bind: SocketAddr,

    /// client に公開する worker の URL
    #[arg(long)]
    pub public_url: String,

    /// controller に接続するための URL
    #[arg(long)]
    pub controller_url: String,

    /// セグメントファイルのキャッシュ先
    #[arg(long)]
    pub cache_dir: PathBuf,

    /// (channel_id, quality) ごとのキャッシュ上限
    #[arg(long)]
    pub cache_limit: u64,
}

impl WorkerConfig {
    /// 設定値を検証する
    pub fn validate(&self) -> Result<(), EdamameError> {
        validate_identifier("worker_id", &self.worker_id)?;
        if self.cache_limit == 0 {
            return Err(EdamameError::BadRequest(
                "cache limit must be positive".to_owned(),
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
            "edamame-worker",
            "--worker-id",
            "worker-a",
            "--bind",
            "127.0.0.1:9001",
            "--public-url",
            "http://127.0.0.1:9001",
            "--controller-url",
            "http://127.0.0.1:9000",
            "--cache-dir",
            ".edamame/worker-a",
            "--cache-limit",
            "12",
        ]
    }

    #[test]
    fn parses_documented_arguments() {
        let config = WorkerConfig::try_parse_from(base_args()).unwrap();
        assert_eq!(config.worker_id, "worker-a");
        assert_eq!(config.cache_limit, 12);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_zero_cache_limit() {
        let mut args = base_args();
        let index = args
            .iter()
            .position(|value| *value == "--cache-limit")
            .unwrap();
        args[index + 1] = "0";
        let config = WorkerConfig::try_parse_from(args).unwrap();
        assert!(config.validate().is_err());
    }
}
