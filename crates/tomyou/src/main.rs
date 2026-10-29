//! tomyou の起動処理

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;
use eframe::egui;

use tomyou::{app::TomyouApp, config::Config, logging};

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

fn main() -> eframe::Result<()> {
    let logging = logging::init();
    tracing::info!(
        version = tomyou::VERSION,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "tomyou starting"
    );
    if let Some(path) = logging.log_path() {
        tracing::info!("log file: {}", path.display());
    }

    let args = Args::parse();
    let explicit_config = args.config.is_some();
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_path().unwrap_or_else(|_| PathBuf::from("tomyou.toml")));
    let (config, startup_error) = match Config::load(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            let message = format!("{error:#}");
            tracing::warn!("failed to load config: {message}");
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
    let log_path = logging.log_path().map(std::path::Path::to_path_buf);
    let title = tomyou::window_title();

    let result = eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            Ok(Box::new(TomyouApp::new(
                cc,
                config,
                config_path,
                runtime,
                startup_error,
                log_path,
            )))
        }),
    );
    // NonBlocking writer のバッファをアプリ終了時に flush する
    drop(logging);
    result
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
