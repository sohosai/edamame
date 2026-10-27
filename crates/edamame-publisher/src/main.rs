//! SRT または RTMP を受け取り、映像をエンコードしてセグメントを作り、暗号化して controller へ送る

mod config;
mod credentials;
mod ffmpeg;
mod metrics;
mod publish;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;

use crate::config::{Cli, PublisherSettings};
use crate::metrics::PublisherMetrics;
use crate::publish::ChannelContext;

fn main() {
    match run() {
        Ok(exit_code) => process::exit(exit_code),
        Err(message) => {
            eprintln!("{message}");
            process::exit(1);
        }
    }
}

/// publisher を起動し、終了コードを返す
fn run() -> Result<i32, String> {
    let cli = Cli::parse();
    let settings = PublisherSettings::load(&cli)?;

    // 実行中は全 channel で同じ鍵を使う。
    // --key-file を指定すると再起動後も同じ鍵を使い、省略すると実行ごとに違う鍵を生成する
    let key = match &settings.key_file {
        Some(path) => {
            let key = publish::load_or_create_key(path)?;
            println!("encryption key file is {}", path.display());
            key
        }
        None => publish::random_key()?,
    };
    println!("encryption key (hex) is {}", publish::hex_encode(&key));
    println!("segment duration is {} ms", settings.segment_duration_ms);
    if settings.stream_credentials_url.is_none() {
        eprintln!(
            "warning: stream credentials are not registered; pass --stream-credentials-url to register them"
        );
    }

    let work_dir = make_work_dir(cli.work_dir.clone())?;
    println!("work directory is {}", work_dir.display());

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("failed to install the signal handler: {error}"))?;
    }

    // OTel の tonic exporter は Tokio runtime を必要とする。
    // publisher 本体は同期スレッドで動かすため、telemetry 用の runtime だけを保持する。
    let telemetry_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to create the telemetry runtime: {error}"))?;
    let _runtime_guard = telemetry_runtime.enter();
    let instance_id = instance_id();
    let mut telemetry = edamame_telemetry::init("edamame-publisher", &instance_id);
    if telemetry.is_enabled() {
        println!("OpenTelemetry metrics enabled (instance: {instance_id})");
    }
    let metrics = Arc::new(PublisherMetrics::new());
    metrics.record_up();

    let failed = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(settings);
    let mut handles = Vec::new();
    for input_index in 0..settings.inputs.len() {
        let context = ChannelContext {
            settings: Arc::clone(&settings),
            input_index,
            key,
            work_dir: work_dir.join(format!("channel-{input_index}")),
            metrics: Arc::clone(&metrics),
        };
        println!(
            "channel={} input_url={} renditions={}",
            context.channel_id(),
            context.input_url(),
            context.renditions().len()
        );
        handles.push(publish::spawn_channel(
            context,
            Arc::clone(&stop),
            Arc::clone(&failed),
        ));
    }

    for handle in handles {
        let _ = handle.join();
    }

    metrics.record_down();
    telemetry.shutdown();
    println!("stopped");
    if failed.load(Ordering::Relaxed) {
        println!("work directory is kept at {}", work_dir.display());
        return Ok(1);
    }
    if let Err(error) = fs::remove_dir_all(&work_dir) {
        println!("failed to remove {}: {error}", work_dir.display());
    }
    Ok(0)
}

/// 作業ディレクトリを作る
///
/// 暗号化前のセグメントファイルを置くため、他の利用者から読めないようにする。
/// 鍵の登録が成功しないまま終了した場合は、終了後も作業ディレクトリを残す。
fn make_work_dir(requested: Option<PathBuf>) -> Result<PathBuf, String> {
    let base = requested.unwrap_or_else(std::env::temp_dir);
    let session = base.join(format!("edamame-publisher-{}", process::id()));
    fs::create_dir_all(&session)
        .map_err(|error| format!("failed to create {}: {error}", session.display()))?;
    fs::set_permissions(&session, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "failed to set permissions on {}: {error}",
            session.display()
        )
    })?;
    Ok(session)
}

/// OpenTelemetry の `service.instance.id` に使う値を返す
fn instance_id() -> String {
    std::env::var("EDAMAME_INSTANCE_ID")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("publisher-{}", process::id()))
}
