//! tomyou の起動処理

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use eframe::egui;
use tracing_subscriber::EnvFilter;

use tomyou::{app::TomyouApp, config::Config};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config_path = Config::default_path().unwrap_or_else(|_| PathBuf::from("tomyou.toml"));
    let (config, startup_error) = match Config::load(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            let message = format!("{error:#}");
            (
                Config::default(),
                Some(format!(
                    "{message}。初期値で起動しました。設定を保存してください: {}",
                    config_path.display()
                )),
            )
        }
    };

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
