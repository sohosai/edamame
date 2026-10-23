//! tomyou の起動処理

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;
use eframe::egui;
use tracing_subscriber::EnvFilter;

use tomyou::{app::TomyouApp, config::Config};

/// tomyou の起動オプション
#[derive(Debug, Parser)]
#[command(
    name = "tomyou",
    version,
    about = "Edamame と ImageFlux へ中継する拠点向けアプリ"
)]
struct Args {
    /// 読み込む TOML 設定ファイル。省略時は OS 標準の設定ディレクトリを使う
    #[arg(short = 'c', long = "config", value_name = "FILE")]
    config: Option<PathBuf>,
}

/// 起動ディレクトリ、実行ファイルの隣、設定ディレクトリの順に .env を探して読み込む。
fn load_dotenv_files() {
    let mut candidates = vec![PathBuf::from(".env")];
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.join(".env"));
    }
    if let Ok(config_path) = Config::default_path()
        && let Some(parent) = config_path.parent()
    {
        candidates.push(parent.join(".env"));
    }
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        match dotenvy::from_path(&path) {
            Ok(_) => tracing::info!("loaded environment file: {}", path.display()),
            Err(error) => tracing::warn!(
                "failed to load environment file {}: {error}",
                path.display()
            ),
        }
        return;
    }
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    load_dotenv_files();

    let args = Args::parse();
    let explicit_config = args.config.is_some();
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_path().unwrap_or_else(|_| PathBuf::from("tomyou.toml")));
    let (config, startup_error) = match Config::load(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            let message = format!("{error:#}");
            let context = if explicit_config {
                "指定した設定ファイルを読み込めませんでした"
            } else {
                "設定ファイルを読み込めませんでした"
            };
            (
                Config::default(),
                Some(format!(
                    "{context}: {}。初期値で起動しました。設定を確認して保存してください: {}",
                    message,
                    config_path.display()
                )),
            )
        }
    };
    tracing::info!("config path: {}", config_path.display());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio ランタイムを作成できません");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "tomyou",
        options,
        Box::new(move |cc| {
            Ok(Box::new(TomyouApp::new(
                cc,
                config,
                config_path,
                runtime,
                startup_error,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_option() {
        let args = Args::try_parse_from(["tomyou", "--config", "custom.toml"])
            .expect("--config を解釈できる");
        assert_eq!(args.config, Some(PathBuf::from("custom.toml")));
    }

    #[test]
    fn config_option_is_optional() {
        let args = Args::try_parse_from(["tomyou"]).expect("引数なしを解釈できる");
        assert!(args.config.is_none());
    }
}
