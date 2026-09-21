//! SRT を受け取り、映像をエンコードしてセグメントを作り、暗号化して controller へ送る

mod config;
mod credentials;
mod ffmpeg;
mod publish;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;

use crate::config::{Cli, PublisherSettings};
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

    // 鍵は実行中は全 channel で同じものを使う。ファイルを指定すると再起動しても変わらない
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

    let failed = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(settings);
    let mut handles = Vec::new();
    for input_index in 0..settings.inputs.len() {
        let context = ChannelContext {
            settings: Arc::clone(&settings),
            input_index,
            key,
            work_dir: work_dir.join(format!("channel-{input_index}")),
        };
        println!(
            "channel={} srt_url={} renditions={}",
            context.channel_id(),
            context.srt_url(),
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
/// セグメントは暗号化する前の状態で一時的に置かれるため、他の利用者から読めないようにする。
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
