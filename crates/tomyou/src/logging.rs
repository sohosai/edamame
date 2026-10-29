//! ログファイルの初期化と秘密情報の伏せ字化

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// ファイル出力を保持するためのガード
///
/// この値をアプリの終了まで保持しないと、バッファに残ったログが失われる。
pub struct Logging {
    log_path: Option<PathBuf>,
    _guard: Option<WorkerGuard>,
}

impl Logging {
    /// 今回の実行で使うログファイル
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

/// ログを初期化する
///
/// ログファイルを作れない場合でもアプリは起動し、コンソール出力だけに切り替える。
pub fn init() -> Logging {
    match init_file() {
        Ok((writer, guard, path)) => {
            let console =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            let file = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tomyou=debug"));
            let _ = tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(false)
                        .with_filter(console),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_ansi(false)
                        .with_file(true)
                        .with_line_number(true)
                        .with_thread_ids(true)
                        .with_writer(writer)
                        .with_filter(file),
                )
                .try_init();
            Logging {
                log_path: Some(path),
                _guard: Some(guard),
            }
        }
        Err(error) => {
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
            eprintln!("ログファイルを初期化できません: {error:#}");
            Logging {
                log_path: None,
                _guard: None,
            }
        }
    }
}

/// 今回の実行用のログファイルを作る
fn init_file() -> Result<(
    tracing_appender::non_blocking::NonBlocking,
    WorkerGuard,
    PathBuf,
)> {
    let dir = log_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("ログディレクトリを作れません: {}", dir.display()))?;
    let path = dir.join(format!(
        "tomyou-{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("ログファイルを作れません: {}", path.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    Ok((writer, guard, path))
}

/// OS 標準のログディレクトリ
pub fn log_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "sohosai", "tomyou")
        .context("ログディレクトリを決められません")?;
    Ok(dirs.data_local_dir().join("logs"))
}

/// URL から認証情報とクエリを伏せ字にする
pub fn redact_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_owned();
    };
    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }
    if url.query().is_some() {
        url.set_query(Some("***"));
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_url_credentials_and_query() {
        let redacted = redact_url("rtmp://user:pass@example.test/live/uni?token=secret");
        assert!(redacted.contains("***"));
        assert!(!redacted.contains("pass"));
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn keeps_urls_without_secrets() {
        assert_eq!(
            redact_url("rtmp://publisher.example.test:1935/live/uni"),
            "rtmp://publisher.example.test:1935/live/uni"
        );
    }

    #[test]
    fn log_directory_is_under_tomyou() {
        let dir = log_dir().expect("ログディレクトリを決められる");
        assert!(dir.to_string_lossy().contains("tomyou"));
        assert!(dir.ends_with("logs"));
    }
}
