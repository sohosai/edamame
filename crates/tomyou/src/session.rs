//! 配信セッションの開始、監視、停止

use anyhow::Context;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc::Sender,
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::watch,
};
use tracing::{error, info, warn};

use crate::{
    config::{Config, StreamCredentialsConfig},
    credentials,
    gstreamer::{self, OutputSpec},
    imageflux::{ImageFluxClient, whip_url},
    obs::{ObsClient, PreviousStream, StreamSnapshot},
    relay::{self, RelayEvent},
    state::State,
    stream_info::StreamInfo,
};

/// GUI へ送るセッションの状態変化
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Starting,
    RelayListening(String),
    ObsConnected,
    ObsStreaming,
    ObsStatus(StreamSnapshot),
    PublisherConnected,
    PublisherDisconnected,
    PlayerConnected,
    StreamInfo(StreamInfo),
    ImageFluxChannel {
        channel_id: String,
        playlist_url: Option<String>,
    },
    ImageFluxPlaylist(String),
    ImageFluxError(String),
    FallbackRegistered,
    FallbackRegistrationFailed(String),
    OutputStarted(String),
    OutputExited(String, String),
    Error(String),
    Log(String),
    Stopped,
}

/// セッションの停止操作
pub struct SessionHandle {
    stop: watch::Sender<bool>,
}

impl SessionHandle {
    /// 停止を要求する。実際の後始末はバックグラウンドで続く。
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

/// セッションを開始する
pub fn start(config: Config, ui: Sender<SessionEvent>) -> SessionHandle {
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        if let Err(error) = run(config, stop_rx, ui.clone()).await {
            error!("session failed: {error:#}");
            let _ = ui.send(SessionEvent::Error(format!("{error:#}")));
        }
        let _ = ui.send(SessionEvent::Stopped);
    });
    SessionHandle { stop: stop_tx }
}

async fn run(
    config: Config,
    mut stop_rx: watch::Receiver<bool>,
    ui: Sender<SessionEvent>,
) -> anyhow::Result<()> {
    let _ = ui.send(SessionEvent::Starting);
    if !config.edamame_enabled() {
        anyhow::bail!("publisher.rtmp_url が未設定です。Edamame へ送信できません");
    }

    let gst_launch = gstreamer::resolve(config.gstreamer.path.as_deref())?;
    let capabilities = gstreamer::inspect(&gst_launch)?;
    let _ = ui.send(SessionEvent::Log(format!(
        "GStreamer: {} ({})",
        gst_launch.display(),
        capabilities.summary()
    )));
    if !capabilities.edamame_ready() {
        anyhow::bail!(
            "GStreamer に Edamame 向け RTMP の要素がありません: {}",
            capabilities.summary()
        );
    }

    let bind: std::net::SocketAddr = config
        .relay
        .bind
        .parse()
        .context("relay.bind を解釈できません")?;
    let input_url = gstreamer::rtmp_input(&config.relay.bind, &config.channel_id);
    let server = format!("rtmp://{}/live", config.relay.bind);
    let key = config.channel_id.clone();

    let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel();
    let relay = relay::start(bind, "live", config.channel_id.clone(), relay_tx).await?;
    let _ = ui.send(SessionEvent::RelayListening(bind.to_string()));

    let (output_stop_tx, output_stop_rx) = watch::channel(false);
    spawn_output(
        gstreamer::edamame_spec(&input_url, &config.publisher.rtmp_url),
        gst_launch.clone(),
        output_stop_rx.clone(),
        ui.clone(),
    );

    let obs = ObsClient::connect(&config.obs).await?;
    let _ = ui.send(SessionEvent::ObsConnected);
    let previous = obs.previous_stream().await?;
    obs.ensure_idle().await?;
    obs.set_local_rtmp(&server, &key).await?;
    if let Err(error) = obs.start().await {
        let _ = obs.restore(&previous).await;
        return Err(error);
    }
    let _ = ui.send(SessionEvent::ObsStreaming);

    let state_path = State::default_path().ok();
    let mut state = match state_path.as_ref() {
        Some(path) => match State::load(path) {
            Ok(state) => state,
            Err(error) => {
                warn!("failed to load state: {error:#}");
                let _ = ui.send(SessionEvent::Log(format!(
                    "状態ファイルを読めません。残存チャンネルの追跡を省略します: {error:#}"
                )));
                State::default()
            }
        },
        None => State::default(),
    };

    let imageflux_client = if config.imageflux_enabled() {
        match ImageFluxClient::new(config.imageflux.token.clone()) {
            Ok(client) => Some(client),
            Err(error) => {
                let _ = ui.send(SessionEvent::ImageFluxError(format!("{error:#}")));
                None
            }
        }
    } else {
        let _ = ui.send(SessionEvent::ImageFluxError(
            "ImageFlux の API トークンが未設定です".to_owned(),
        ));
        None
    };
    if config.stream_credentials.enabled() && imageflux_client.is_none() {
        let _ = ui.send(SessionEvent::FallbackRegistrationFailed(
            "ImageFlux の API クライアントを作れません".to_owned(),
        ));
    }

    if let Some(client) = &imageflux_client {
        let mut remaining = Vec::new();
        for channel_id in state.imageflux_channel_ids.drain(..) {
            match client.delete_channel(&channel_id).await {
                Ok(()) => {
                    let _ = ui.send(SessionEvent::Log(format!(
                        "前回の ImageFlux チャンネルを削除: {channel_id}"
                    )));
                }
                Err(error) => {
                    warn!("failed to delete old ImageFlux channel: {error:#}");
                    let _ = ui.send(SessionEvent::ImageFluxError(format!(
                        "前回の ImageFlux チャンネル {channel_id} を削除できません: {error:#}"
                    )));
                    remaining.push(channel_id);
                }
            }
        }
        state.imageflux_channel_ids = remaining;
        if let Some(path) = &state_path
            && let Err(error) = state.save(path)
        {
            warn!("failed to persist state: {error:#}");
        }
    }

    let mut imageflux_channel: Option<String> = None;
    if let Some(client) = &imageflux_client {
        match prepare_imageflux(
            client,
            &config,
            &gst_launch,
            &capabilities,
            &input_url,
            output_stop_rx.clone(),
            ui.clone(),
        )
        .await
        {
            Ok(channel_id) => {
                state.imageflux_channel_ids.push(channel_id.clone());
                if let Some(path) = &state_path
                    && let Err(error) = state.save(path)
                {
                    warn!("failed to persist state: {error:#}");
                    let _ = ui.send(SessionEvent::Error(format!(
                        "ImageFlux チャンネル ID を保存できません: {error:#}"
                    )));
                }
                imageflux_channel = Some(channel_id);
            }
            Err(error) => {
                warn!("ImageFlux is unavailable: {error:#}");
                let _ = ui.send(SessionEvent::ImageFluxError(format!("{error:#}")));
                if config.stream_credentials.enabled() {
                    let _ = ui.send(SessionEvent::FallbackRegistrationFailed(format!(
                        "ImageFlux を準備できません: {error:#}"
                    )));
                }
            }
        }
    }

    let mut status_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            _ = status_interval.tick() => {
                if let Ok(status) = obs.status().await {
                    let _ = ui.send(SessionEvent::ObsStatus(status));
                }
            }
            event = relay_rx.recv() => {
                match event {
                    Some(RelayEvent::PublisherConnected) => { let _ = ui.send(SessionEvent::PublisherConnected); }
                    Some(RelayEvent::PublisherDisconnected) => { let _ = ui.send(SessionEvent::PublisherDisconnected); }
                    Some(RelayEvent::PlayerConnected) => { let _ = ui.send(SessionEvent::PlayerConnected); }
                    Some(RelayEvent::StreamInfo(info)) => { let _ = ui.send(SessionEvent::StreamInfo(info)); }
                    Some(RelayEvent::Error(message)) => { let _ = ui.send(SessionEvent::Error(message)); }
                    Some(RelayEvent::Listening(address)) => { let _ = ui.send(SessionEvent::RelayListening(address.to_string())); }
                    Some(RelayEvent::PlayerDisconnected) => {}
                    None => {}
                }
            }
        }
    }

    let _ = output_stop_tx.send(true);
    if let Err(error) = obs.stop().await {
        warn!("failed to stop OBS: {error:#}");
    }
    if obs.is_local_rtmp(&server, &key).await
        && let Err(error) = restore_stream_settings(&obs, &previous).await
    {
        warn!("failed to restore OBS stream settings: {error:#}");
    }
    relay.stop();

    if let (Some(client), Some(channel_id)) =
        (imageflux_client.as_ref(), imageflux_channel.as_ref())
    {
        match client.delete_channel(channel_id).await {
            Ok(()) => {
                state.imageflux_channel_ids.retain(|id| id != channel_id);
                if let Some(path) = &state_path
                    && let Err(error) = state.save(path)
                {
                    warn!("failed to persist state: {error:#}");
                }
            }
            Err(error) => {
                warn!("failed to delete ImageFlux channel: {error:#}");
                let _ = ui.send(SessionEvent::ImageFluxError(format!(
                    "ImageFlux チャンネル {channel_id} を削除できません: {error:#}"
                )));
            }
        }
    }

    // 出力タスクが GStreamer を終了させる時間を少しだけ待つ
    tokio::time::sleep(Duration::from_millis(300)).await;
    info!("session stopped");
    Ok(())
}

/// OBS の配信先を元へ戻す
///
/// OBS は停止要求を受理してから実際に出力が止まるまで配信先を変更できない。
/// 停止直後に戻そうとすると OutputRunning で失敗するため、少しの間だけ再試行する。
async fn restore_stream_settings(obs: &ObsClient, previous: &PreviousStream) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match obs.restore(previous).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn prepare_imageflux(
    client: &ImageFluxClient,
    config: &Config,
    gst_launch: &Path,
    capabilities: &gstreamer::GstCapabilities,
    input_url: &str,
    stop_rx: watch::Receiver<bool>,
    ui: Sender<SessionEvent>,
) -> anyhow::Result<String> {
    if !capabilities.imageflux_ready() {
        anyhow::bail!(
            "GStreamer に ImageFlux 向け WHIP の要素がありません: {}",
            capabilities.summary()
        );
    }
    let decoder = capabilities
        .audio_decoder()
        .context("AAC デコーダがありません")?;
    let key_uri = config.imageflux_key_uri()?;
    let created = client
        .create_channel(
            &config.imageflux.hls,
            &key_uri,
            &config.imageflux.event_webhook_url,
        )
        .await?;
    let whip = match whip_url(&created.sora_url, &created.channel_id) {
        Ok(whip) => whip,
        Err(error) => {
            // Sora URL が不正でもチャンネルを作ったままにしない
            let _ = client.delete_channel(&created.channel_id).await;
            return Err(error);
        }
    };
    let _ = ui.send(SessionEvent::ImageFluxChannel {
        channel_id: created.channel_id.clone(),
        playlist_url: created.playlist_url.clone(),
    });

    spawn_output(
        gstreamer::imageflux_spec(input_url, &whip, decoder),
        gst_launch.to_path_buf(),
        stop_rx.clone(),
        ui.clone(),
    );

    // プレイリストの取得には ImageFlux のチャンネル ID を使い、
    // fallback 登録には live2025-server が知っているチャンネル名を使う
    let imageflux_channel_id = created.channel_id.clone();
    let registration_channel_id = config.channel_id.clone();
    let playlist_client = client.clone();
    let registration = config
        .stream_credentials
        .enabled()
        .then(|| config.stream_credentials.clone());
    let registration_enabled = registration.is_some();
    let registration_stop = stop_rx;
    tokio::spawn(async move {
        match playlist_client
            .wait_for_playlist(&imageflux_channel_id)
            .await
        {
            Ok(url) => {
                let _ = ui.send(SessionEvent::ImageFluxPlaylist(url.clone()));
                if let Some(settings) = registration {
                    register_fallback(
                        playlist_client,
                        settings,
                        registration_channel_id,
                        url,
                        registration_stop,
                        ui,
                    )
                    .await;
                }
            }
            Err(error) => {
                let _ = ui.send(SessionEvent::ImageFluxError(format!("{error:#}")));
                if registration_enabled {
                    let _ = ui.send(SessionEvent::FallbackRegistrationFailed(format!(
                        "ImageFlux の HLS プレイリストを取得できません: {error:#}"
                    )));
                }
            }
        }
    });

    Ok(created.channel_id)
}

/// ImageFlux を fallback 配信として live2025-server へ登録する
async fn register_fallback(
    imageflux: ImageFluxClient,
    settings: StreamCredentialsConfig,
    channel_id: String,
    playlist_url: String,
    mut stop: watch::Receiver<bool>,
    ui: Sender<SessionEvent>,
) {
    let Some(key) = fetch_encrypt_key_with_retry(&imageflux, &playlist_url, &mut stop, &ui).await
    else {
        return;
    };
    let client = match credentials::client() {
        Ok(client) => client,
        Err(error) => {
            let _ = ui.send(SessionEvent::FallbackRegistrationFailed(format!(
                "fallback 登録の HTTP クライアントを作れません: {error:#}"
            )));
            return;
        }
    };
    let endpoint = credentials::endpoint(&settings.base_url);
    let body = credentials::body(&channel_id, &playlist_url, &key);
    let ingest_token = settings.ingest_token.trim();
    let log_ui = ui.clone();
    let log = move |message: String| {
        let _ = log_ui.send(SessionEvent::Log(message));
    };
    match credentials::register_until_success(
        &client,
        &endpoint,
        ingest_token,
        &body,
        &mut stop,
        log,
    )
    .await
    {
        Ok(()) => {
            info!("fallback stream credentials registered");
            let _ = ui.send(SessionEvent::FallbackRegistered);
        }
        Err(error) => {
            if !*stop.borrow() {
                warn!("failed to register fallback stream credentials: {error:#}");
                let _ = ui.send(SessionEvent::FallbackRegistrationFailed(format!(
                    "{error:#}"
                )));
            }
        }
    }
}

/// プレイリストの内容が準備できるまで、鍵取得を少しだけ再試行する
async fn fetch_encrypt_key_with_retry(
    imageflux: &ImageFluxClient,
    playlist_url: &str,
    stop: &mut watch::Receiver<bool>,
    ui: &Sender<SessionEvent>,
) -> Option<String> {
    const KEY_FETCH_ATTEMPTS: u32 = 3;
    let mut delay = Duration::from_secs(2);
    for attempt in 0..KEY_FETCH_ATTEMPTS {
        match imageflux.fetch_encrypt_key(playlist_url).await {
            Ok(key) => return Some(key),
            Err(error) if attempt + 1 < KEY_FETCH_ATTEMPTS => {
                let _ = ui.send(SessionEvent::Log(format!(
                    "ImageFlux の暗号鍵を取得できません。再試行します: {error:#}"
                )));
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stop.changed() => return None,
                }
                delay *= 2;
            }
            Err(error) => {
                if !*stop.borrow() {
                    let _ = ui.send(SessionEvent::FallbackRegistrationFailed(format!(
                        "ImageFlux の暗号鍵を取得できません: {error:#}"
                    )));
                }
                return None;
            }
        }
    }
    None
}

fn spawn_output(
    spec: OutputSpec,
    gst_launch: PathBuf,
    stop_rx: watch::Receiver<bool>,
    ui: Sender<SessionEvent>,
) {
    tokio::spawn(async move {
        run_output(spec, gst_launch, stop_rx, ui).await;
    });
}

async fn run_output(
    spec: OutputSpec,
    gst_launch: PathBuf,
    mut stop_rx: watch::Receiver<bool>,
    ui: Sender<SessionEvent>,
) {
    let name = spec.kind.name().to_owned();
    let mut backoff = 1_u64;
    loop {
        if *stop_rx.borrow() {
            return;
        }
        let mut command = Command::new(&gst_launch);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 中継のたびにコンソールウィンドウが開かないようにする
        gstreamer::hide_console_window(command.as_std_mut());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = ui.send(SessionEvent::OutputExited(
                    name.clone(),
                    format!("起動できません: {error}"),
                ));
                if wait_backoff(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        let _ = ui.send(SessionEvent::OutputStarted(name.clone()));
        if let Some(stderr) = child.stderr.take() {
            let ui = ui.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if is_useful_gst_line(&line) {
                        let _ = ui.send(SessionEvent::Log(format!("{name}: {line}")));
                    }
                }
            });
        }
        let status = tokio::select! {
            result = child.wait() => result,
            _ = stop_rx.changed() => {
                let _ = child.kill().await;
                return;
            }
        };
        let message = match status {
            Ok(status) => format!("終了: {status}"),
            Err(error) => format!("待機に失敗: {error}"),
        };
        let _ = ui.send(SessionEvent::OutputExited(name.clone(), message));
        if wait_backoff(&mut stop_rx, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(30);
    }
}

/// 画面に出す価値のある GStreamer のエラー行か
fn is_useful_gst_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "error",
        "failed",
        "invalid",
        "warning",
        "refused",
        "timed out",
        "unreachable",
        "no route",
        "handshake",
        "broken pipe",
        "not-negotiated",
        "ice",
        "dtls",
        "whipclientsink",
        "setup elements",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// 停止要求を待ちながらバックオフする。停止要求なら true。
async fn wait_backoff(stop_rx: &mut watch::Receiver<bool>, seconds: u64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(seconds)) => false,
        _ = stop_rx.changed() => *stop_rx.borrow(),
    }
}
