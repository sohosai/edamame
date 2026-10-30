//! egui の画面

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, TryRecvError},
    },
    time::Duration,
};

use eframe::egui;
use tracing::warn;

use crate::{
    config::{CHANNELS, Config},
    obs::StreamSnapshot,
    session::{self, SessionEvent, SessionHandle},
    stream_info::StreamInfo,
};

/// 画面全体の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Starting,
    Streaming,
    Stopping,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "待機中",
            Self::Starting => "開始中",
            Self::Streaming => "配信中",
            Self::Stopping => "停止中",
            Self::Error => "エラー",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Idle => Tone::Idle,
            Self::Starting | Self::Stopping => Tone::Warn,
            Self::Streaming => Tone::Good,
            Self::Error => Tone::Bad,
        }
    }
}

/// 状態の意味を表す色
///
/// 明るい背景に純色を置くと読みにくいため、テーマの明暗に合わせて濃さを変える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// 待機中、未接続
    Idle,
    /// 正常
    Good,
    /// 注意、遷移中
    Warn,
    /// 異常
    Bad,
}

impl Tone {
    fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        let dark = visuals.dark_mode;
        match self {
            Self::Idle => visuals.weak_text_color(),
            // 暗い背景では淡く、明るい背景では濃くする
            Self::Good if dark => egui::Color32::from_rgb(110, 225, 135),
            Self::Good => egui::Color32::from_rgb(0, 110, 40),
            Self::Warn if dark => egui::Color32::from_rgb(255, 200, 70),
            Self::Warn => egui::Color32::from_rgb(140, 88, 0),
            Self::Bad if dark => egui::Color32::from_rgb(255, 120, 110),
            Self::Bad => egui::Color32::from_rgb(180, 25, 25),
        }
    }
}

/// 状態の色を付けたラベル
///
/// 色だけに頼らず、太字にして小さな文字でも読み取れるようにする。
fn tone_label(ui: &mut egui::Ui, tone: Tone, text: impl Into<String>) {
    let color = tone.color(ui.visuals());
    ui.label(egui::RichText::new(text.into()).color(color).strong());
}

/// 状態を色付きのバッジで示す
fn status_badge(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let color = tone.color(ui.visuals());
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.16))
        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.6)))
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(color).strong());
        });
}

/// 設定ウィンドウで押された操作
#[derive(Debug, Default)]
struct SettingsActions {
    close: bool,
    load: bool,
    save: bool,
    open_config: bool,
    open_log: bool,
}

/// tomyou のメイン画面
pub struct TomyouApp {
    config: Config,
    config_path: PathBuf,
    config_path_draft: String,
    runtime: tokio::runtime::Runtime,
    events_tx: Sender<SessionEvent>,
    events_rx: Receiver<SessionEvent>,
    session: Option<SessionHandle>,
    status: Status,
    logs: VecDeque<String>,
    error: Option<String>,
    startup_error: Option<String>,
    imageflux_channel: Option<String>,
    imageflux_playlist: Option<String>,
    fallback_registered: bool,
    fallback_error: Option<String>,
    publisher_connected: bool,
    imageflux_output: bool,
    edamame_output: bool,
    edamame_connecting: bool,
    edamame_error: Option<String>,
    log_path: Option<PathBuf>,
    show_settings: bool,
    show_license: bool,
    gstreamer_path_draft: String,
    publisher_hosts_draft: String,
    obs_snapshot: Option<StreamSnapshot>,
    stream_info: Option<StreamInfo>,
    /// 今 Edamame へ送っている送信先
    edamame_destination: Option<String>,
}

impl TomyouApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: PathBuf,
        runtime: tokio::runtime::Runtime,
        startup_error: Option<String>,
        log_path: Option<PathBuf>,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let config_path_draft = config_path.display().to_string();
        let gstreamer_path_draft = config
            .gstreamer
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let publisher_hosts_draft = config.publisher.hosts().join("\n");
        Self {
            config,
            config_path,
            config_path_draft,
            runtime,
            events_tx,
            events_rx,
            session: None,
            status: Status::Idle,
            logs: VecDeque::new(),
            error: None,
            startup_error,
            imageflux_channel: None,
            imageflux_playlist: None,
            fallback_registered: false,
            fallback_error: None,
            publisher_connected: false,
            imageflux_output: false,
            edamame_output: false,
            edamame_connecting: false,
            edamame_error: None,
            log_path,
            show_settings: false,
            show_license: false,
            gstreamer_path_draft,
            publisher_hosts_draft,
            obs_snapshot: None,
            stream_info: None,
            edamame_destination: None,
        }
    }

    fn log(&mut self, message: impl Into<String>) {
        let message = message.into();
        // 画面のログはファイルにも残す。コンソールは info 以上だけなので、
        // 詳細ログはファイル側の debug フィルタで読む
        tracing::debug!(target: "tomyou::ui", "{message}");
        if self.logs.len() >= 2000 {
            self.logs.pop_front();
        }
        self.logs.push_back(message);
    }

    fn start(&mut self) {
        if self.session.is_some() {
            return;
        }
        if let Err(error) = self.config.validate() {
            self.error = Some(format!("{error:#}"));
            self.status = Status::Error;
            self.log(format!("設定エラー: {error:#}"));
            return;
        }
        self.error = None;
        self.imageflux_channel = None;
        self.imageflux_playlist = None;
        self.fallback_registered = false;
        self.fallback_error = None;
        self.stream_info = None;
        self.publisher_connected = false;
        self.imageflux_output = false;
        self.edamame_output = false;
        self.edamame_connecting = false;
        self.edamame_error = None;
        self.edamame_destination = None;
        self.obs_snapshot = None;
        self.status = Status::Starting;
        self.log("配信を開始します");
        let _guard = self.runtime.enter();
        self.session = Some(session::start(self.config.clone(), self.events_tx.clone()));
    }

    fn stop(&mut self) {
        if self.session.is_some() {
            self.status = Status::Stopping;
            self.log("停止を要求しました");
            if let Some(session) = &self.session {
                session.stop();
            }
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.events_rx.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn handle_event(&mut self, event: SessionEvent) {
        tracing::debug!(target: "tomyou::ui", event = ?event, "session event");
        match event {
            SessionEvent::Starting => {
                self.status = Status::Starting;
                self.log("セッションを開始しています");
            }
            SessionEvent::RelayListening(address) => {
                self.log(format!("RTMP 受信を開始: {address}"));
            }
            SessionEvent::ObsConnected => {
                self.log("OBS に接続しました");
            }
            SessionEvent::ObsStreaming => {
                self.status = Status::Streaming;
                self.log("OBS の配信を開始しました");
            }
            SessionEvent::ObsStatus(status) => {
                self.obs_snapshot = Some(status);
            }
            SessionEvent::PublisherConnected => {
                self.publisher_connected = true;
                self.stream_info = None;
                self.log("OBS からの RTMP 入力を受け付けました");
            }
            SessionEvent::PublisherDisconnected => {
                self.publisher_connected = false;
                self.stream_info = None;
                self.log("OBS からの RTMP 入力が切れました");
            }
            SessionEvent::PlayerConnected => {
                self.log("ローカルの中継クライアントが接続しました");
            }
            SessionEvent::StreamInfo(info) => {
                if self.session.is_some() {
                    self.stream_info = Some(info);
                }
            }
            SessionEvent::ImageFluxChannel {
                channel_id,
                playlist_url,
            } => {
                self.imageflux_channel = Some(channel_id.clone());
                if let Some(url) = playlist_url {
                    self.imageflux_playlist = Some(url);
                }
                self.log(format!("ImageFlux チャンネルを作成: {channel_id}"));
            }
            SessionEvent::ImageFluxPlaylist(url) => {
                self.imageflux_playlist = Some(url.clone());
                self.log(format!("ImageFlux の HLS URL を取得: {url}"));
            }
            SessionEvent::ImageFluxError(error) => {
                self.log(format!("ImageFlux: {error}"));
            }
            SessionEvent::FallbackRegistered => {
                if self.session.is_some() {
                    self.fallback_registered = true;
                    self.fallback_error = None;
                    self.log("ImageFlux を fallback 配信として登録しました");
                }
            }
            SessionEvent::FallbackRegistrationFailed(error) => {
                if self.session.is_some() {
                    self.fallback_registered = false;
                    self.fallback_error = Some(error.clone());
                    self.log(format!("fallback 登録に失敗: {error}"));
                }
            }
            SessionEvent::OutputConnecting(name) => {
                if self.session.is_some() && name == "Edamame" {
                    self.edamame_output = false;
                    self.edamame_connecting = true;
                    self.edamame_error = None;
                }
                self.log(format!("{name} への接続を確認しています"));
            }
            SessionEvent::OutputStarted(name) => {
                // セッション終了の通知より後に届くことがあるため、配信中だけ反映する
                if self.session.is_some() {
                    if name == "Edamame" {
                        self.edamame_output = true;
                        self.edamame_connecting = false;
                        self.edamame_error = None;
                    } else {
                        self.imageflux_output = true;
                    }
                }
                self.log(format!("{name} への送信を開始しました"));
            }
            SessionEvent::OutputError(name, message) => {
                if self.session.is_some() {
                    if name == "Edamame" {
                        self.edamame_output = false;
                        self.edamame_connecting = false;
                        self.edamame_error = Some(message.clone());
                    } else {
                        self.imageflux_output = false;
                    }
                }
                self.log(format!("{name} の送信エラー: {message}"));
            }
            SessionEvent::OutputExited(name, message) => {
                if name == "Edamame" {
                    self.edamame_output = false;
                    self.edamame_connecting = false;
                } else {
                    self.imageflux_output = false;
                }
                self.log(format!("{name} の送信が終了: {message}"));
            }
            SessionEvent::EdamameDestination(destination) => {
                if self.session.is_some() {
                    self.edamame_destination = Some(destination);
                }
            }
            SessionEvent::Error(error) => {
                self.status = Status::Error;
                self.error = Some(error.clone());
                self.log(format!("エラー: {error}"));
            }
            SessionEvent::Log(message) => {
                self.log(message);
            }
            SessionEvent::Stopped => {
                self.status = Status::Idle;
                self.session = None;
                self.publisher_connected = false;
                self.imageflux_output = false;
                self.edamame_output = false;
                self.edamame_connecting = false;
                self.edamame_error = None;
                self.edamame_destination = None;
                self.fallback_registered = false;
                self.fallback_error = None;
                self.stream_info = None;
                self.obs_snapshot = None;
                self.log("停止しました");
            }
        }
    }

    fn config_path_from_draft(&self) -> PathBuf {
        let path = self.config_path_draft.trim();
        if path.is_empty() {
            self.config_path.clone()
        } else {
            PathBuf::from(path)
        }
    }

    fn save_config(&mut self) {
        self.config.gstreamer.path = if self.gstreamer_path_draft.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.gstreamer_path_draft.trim()))
        };
        // 画面では 1 行に 1 つ並べているので、空行を落として保存する
        self.config.publisher.hosts = self
            .publisher_hosts_draft
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let path = self.config_path_from_draft();
        match self.config.save(&path) {
            Ok(()) => {
                self.config_path = path.clone();
                self.config_path_draft = path.display().to_string();
                self.startup_error = None;
                self.error = None;
                self.log(format!("設定を保存しました: {}", path.display()));
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                self.log(format!("設定を保存できません: {error:#}"));
            }
        }
    }

    fn open_config_file(&mut self) {
        let path = self.config_path_from_draft();
        if path.as_os_str().is_empty() {
            self.error = Some("設定ファイルのパスが空です".to_owned());
            return;
        }
        if !path.is_file() {
            self.save_config();
            if self.error.is_some() {
                return;
            }
        }
        match open::that(&path) {
            Ok(()) => {
                self.error = None;
                self.log(format!("設定ファイルを開きました: {}", path.display()));
            }
            Err(error) => {
                self.error = Some(format!("設定ファイルを開けません: {error:#}"));
                self.log(format!("設定ファイルを開けません: {error:#}"));
            }
        }
    }

    fn open_log_file(&mut self) {
        let Some(path) = self.log_path.clone() else {
            return;
        };
        match open::that(&path) {
            Ok(()) => {
                self.error = None;
                self.log(format!("ログファイルを開きました: {}", path.display()));
            }
            Err(error) => {
                self.error = Some(format!("ログファイルを開けません: {error:#}"));
                self.log(format!("ログファイルを開けません: {error:#}"));
            }
        }
    }

    fn load_config_from_draft(&mut self) {
        let path = self.config_path_from_draft();
        if path.as_os_str().is_empty() {
            self.error = Some("設定ファイルのパスが空です".to_owned());
            return;
        }
        match Config::load(&path) {
            Ok(config) => {
                self.gstreamer_path_draft = config
                    .gstreamer
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                self.publisher_hosts_draft = config.publisher.hosts().join("\n");
                self.config = config;
                self.config_path = path.clone();
                self.config_path_draft = path.display().to_string();
                self.startup_error = None;
                self.error = None;
                self.log(format!("設定ファイルを読み込みました: {}", path.display()));
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                self.log(format!("設定を読み込めません: {error:#}"));
            }
        }
    }
}

impl eframe::App for TomyouApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        ctx.request_repaint_after(Duration::from_millis(250));

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("tomyou v{}", crate::VERSION));
                ui.separator();
                ui.label(format!("チャンネル: {}", self.config.channel_id));
                ui.separator();
                status_badge(ui, self.status.label(), self.status.tone());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("ライセンス").clicked() {
                        self.show_license = true;
                    }
                    if ui.button("設定").clicked() {
                        self.show_settings = true;
                    }
                });
            });
        });

        egui::TopBottomPanel::bottom("logs")
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("ログ");
                    if ui.button("クリア").clicked() {
                        self.logs.clear();
                    }
                    if ui.button("コピー").clicked() {
                        let text = self.logs.iter().cloned().collect::<Vec<_>>().join("\n");
                        ui.ctx().copy_text(text);
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for line in &self.logs {
                            ui.monospace(line);
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            if let Some(error) = &self.startup_error {
                tone_label(ui, Tone::Bad, format!("起動時の設定: {error}"));
                ui.add_space(4.0);
            }
            if let Some(error) = &self.error {
                tone_label(ui, Tone::Bad, error.clone());
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let can_start = self.session.is_none() && self.status != Status::Starting;
                let can_stop = self.session.is_some() && self.status != Status::Stopping;
                if ui
                    .add_enabled(can_start, egui::Button::new("配信を開始"))
                    .clicked()
                {
                    self.start();
                }
                if ui
                    .add_enabled(can_stop, egui::Button::new("配信を停止"))
                    .clicked()
                {
                    self.stop();
                }
            });
            ui.add_space(8.0);
            let relay_url = self.config.relay_url();
            let relay_server = self.config.relay_server();
            // 0.0.0.0 や :: は待ち受け専用で、そのままでは接続先に使えない
            let relay_bind_is_unspecified = self.config.relay.bind.starts_with("0.0.0.0:")
                || self.config.relay.bind.starts_with("[::]:");
            ui.group(|ui| {
                ui.label(egui::RichText::new("入力").strong());
                egui::Grid::new("input-status")
                    .num_columns(2)
                    .spacing([18.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("OBS");
                        ui.label(if self.status == Status::Streaming {
                            "配信中"
                        } else if self.session.is_some() {
                            "接続中"
                        } else {
                            "未接続"
                        });
                        ui.end_row();
                        ui.label("RTMP 受信");
                        ui.label(if self.publisher_connected {
                            "受信中"
                        } else {
                            "待機中"
                        });
                        ui.end_row();
                        ui.label("RTMP URL");
                        ui.horizontal(|ui| {
                            ui.monospace(&relay_url);
                            if ui.button("コピー").clicked() {
                                ui.ctx().copy_text(relay_url.clone());
                            }
                        });
                        ui.end_row();
                        ui.label("パスワード");
                        ui.label("なし（認証なし）");
                        ui.end_row();
                        ui.label("チャンネル");
                        ui.label(&self.config.channel_id);
                        ui.end_row();
                        ui.label("映像");
                        match self
                            .stream_info
                            .as_ref()
                            .and_then(|info| info.video.as_ref())
                        {
                            Some(info) => {
                                let problem = info.problem();
                                let text = match &problem {
                                    Some(problem) => {
                                        format!("{}（{}）", info.summary(), problem)
                                    }
                                    None => info.summary(),
                                };
                                let tone = if problem.is_some() {
                                    Tone::Bad
                                } else {
                                    Tone::Good
                                };
                                tone_label(ui, tone, text);
                            }
                            None => {
                                tone_label(ui, Tone::Idle, "待機中");
                            }
                        }
                        ui.end_row();
                        ui.label("音声");
                        match self
                            .stream_info
                            .as_ref()
                            .and_then(|info| info.audio.as_ref())
                        {
                            Some(info) => {
                                let problem = info.problem();
                                let text = match &problem {
                                    Some(problem) => {
                                        format!("{}（{}）", info.summary(), problem)
                                    }
                                    None => info.summary(),
                                };
                                let tone = if problem.is_some() {
                                    Tone::Bad
                                } else {
                                    Tone::Good
                                };
                                tone_label(ui, tone, text);
                            }
                            None => {
                                tone_label(ui, Tone::Idle, "待機中");
                            }
                        }
                        ui.end_row();
                        ui.label("B フレーム");
                        match &self.stream_info {
                            Some(info) => {
                                let tone = if info.video_frames == 0 {
                                    Tone::Idle
                                } else if info.bframe_problem().is_some() {
                                    Tone::Bad
                                } else {
                                    Tone::Good
                                };
                                tone_label(ui, tone, info.bframe_text());
                            }
                            None => {
                                tone_label(ui, Tone::Idle, "待機中");
                            }
                        }
                        ui.end_row();
                        ui.label("キーフレーム間隔");
                        match &self.stream_info {
                            Some(info) => match info.keyframe_text() {
                                Some(text) => {
                                    let tone = if info.keyframe_problem().is_some() {
                                        Tone::Bad
                                    } else {
                                        Tone::Good
                                    };
                                    tone_label(ui, tone, text);
                                }
                                None => {
                                    tone_label(ui, Tone::Idle, "計測中");
                                }
                            },
                            None => {
                                tone_label(ui, Tone::Idle, "待機中");
                            }
                        }
                        ui.end_row();
                        ui.label("入力チェック");
                        match &self.stream_info {
                            Some(info) if info.is_ready() => {
                                if info.video_frames == 0
                                    || info.keyframe_interval_ms.is_none()
                                {
                                    tone_label(ui, Tone::Idle, "計測中");
                                } else if info.problems().is_empty() {
                                    tone_label(ui, Tone::Good, "OK");
                                } else {
                                    tone_label(ui, Tone::Bad, "要確認");
                                }
                            }
                            _ => {
                                tone_label(ui, Tone::Idle, "待機中");
                            }
                        }
                        ui.end_row();
                    });
                ui.label(format!(
                    "OBS で手動設定する場合は、サーバー {relay_server}、ストリームキー {} を指定します。",
                    self.config.channel_id
                ));
                ui.label("RTMP URL は配信中だけ有効です。");
                if relay_bind_is_unspecified {
                    tone_label(
                        ui,
                        Tone::Warn,
                        "0.0.0.0 は待ち受け専用です。接続先にはこの PC の IP を使ってください。",
                    );
                }
            });
            ui.add_space(8.0);
            ui.columns(2, |columns| {
                columns[0].group(|ui| {
                    ui.label(egui::RichText::new("Edamame（主系）").strong());
                    let (text, tone) = if self.session.is_none() {
                        ("停止", Tone::Idle)
                    } else if self.edamame_output {
                        ("送信中", Tone::Good)
                    } else if self.edamame_error.is_some() {
                        ("接続失敗", Tone::Bad)
                    } else if self.edamame_connecting {
                        ("接続確認中", Tone::Warn)
                    } else {
                        ("再接続中", Tone::Warn)
                    };
                    tone_label(ui, tone, text);
                    if self.session.is_some()
                        && let Some(error) = &self.edamame_error
                    {
                        tone_label(ui, Tone::Bad, error.clone());
                    }
                    let urls = self
                        .config
                        .publisher
                        .urls(&self.config.channel_id)
                        .unwrap_or_default();
                    let destination = self
                        .edamame_destination
                        .clone()
                        .or_else(|| urls.first().cloned())
                        .unwrap_or_default();
                    if destination.is_empty() {
                        ui.label("送信先: 未設定");
                    } else if urls.len() > 1 {
                        ui.label(format!(
                            "送信先: {destination}（{} 件中 {} 番目）",
                            urls.len(),
                            urls.iter().position(|url| *url == destination).map_or(1, |index| index + 1)
                        ));
                    } else {
                        ui.label(format!("送信先: {destination}"));
                    }
                });
                columns[1].group(|ui| {
                    ui.label(egui::RichText::new("ImageFlux（予備系）").strong());
                    let (text, tone) = if self.imageflux_output {
                        ("WHIP 送信中", Tone::Good)
                    } else if self.session.is_some() {
                        ("接続待ち / 再接続中", Tone::Warn)
                    } else {
                        ("停止", Tone::Idle)
                    };
                    tone_label(ui, tone, text);
                    if let Some(channel) = &self.imageflux_channel {
                        ui.label(format!("チャンネル: {channel}"));
                    }
                    if let Some(url) = &self.imageflux_playlist {
                        ui.horizontal(|ui| {
                            ui.label("HLS URL:");
                            ui.monospace(url);
                            if ui.button("コピー").clicked() {
                                ui.ctx().copy_text(url.clone());
                            }
                        });
                    }
                    let (fallback_status, fallback_tone) =
                        if !self.config.stream_credentials.enabled() {
                            ("未設定", Tone::Idle)
                        } else if self.fallback_error.is_some() {
                            ("失敗", Tone::Bad)
                        } else if self.fallback_registered {
                            ("登録済み", Tone::Good)
                        } else if self.session.is_some() {
                            ("待機中", Tone::Warn)
                        } else {
                            ("未登録", Tone::Idle)
                        };
                    tone_label(
                        ui,
                        fallback_tone,
                        format!("fallback 登録: {fallback_status}"),
                    );
                    if let Some(error) = &self.fallback_error {
                        tone_label(ui, Tone::Bad, error.clone());
                    }
                });
            });
            ui.add_space(8.0);
            ui.label(
                "入力の期待値: H.264 / 1920x1080 / AAC / B フレームなし / キーフレーム間隔 2 秒推奨。赤は要確認。",
            );
            if let Some(status) = &self.obs_snapshot {
                ui.label(format!(
                    "OBS 経過時間: {} 秒 (timecode {} 秒) / 転送: {:.1} MiB / フレーム: {} / 落ちたフレーム: {}",
                    status.duration_seconds,
                    status.timecode_seconds,
                    status.bytes as f64 / (1024.0 * 1024.0),
                    status.total_frames,
                    status.skipped_frames
                ));
                if status.reconnecting {
                    tone_label(ui, Tone::Warn, "OBS が再接続中です");
                }
            }
        });

        self.settings_window(ctx);
        self.license_window(ctx);
    }
}

impl TomyouApp {
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut actions = SettingsActions::default();
        let mut open = true;
        // 設定は別のネイティブウィンドウとして開き、本体の外へ動かせるようにする
        let viewport_id = egui::ViewportId::from_hash_of("tomyou-settings");
        let builder = egui::ViewportBuilder::default()
            .with_title("設定 - tomyou")
            .with_inner_size([760.0, 720.0])
            .with_min_inner_size([520.0, 320.0]);
        ctx.show_viewport_immediate(viewport_id, builder, |ctx, class| {
            match class {
                egui::ViewportClass::Embedded => {
                    // 別ウィンドウに対応しないバックエンドでは本体の中に出す
                    egui::Window::new("設定")
                        .open(&mut open)
                        .default_width(720.0)
                        .default_height(600.0)
                        .show(ctx, |ui| {
                            self.settings_scroll_ui(ui, &mut actions);
                        });
                }
                _ => {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        self.settings_scroll_ui(ui, &mut actions);
                    });
                }
            }
            if ctx.input(|input| input.viewport().close_requested()) {
                open = false;
            }
        });
        self.show_settings = open && !actions.close;
        if actions.open_config {
            self.open_config_file();
        }
        if actions.open_log {
            self.open_log_file();
        }
        if actions.load {
            self.load_config_from_draft();
        }
        if actions.save {
            self.save_config();
            if self.error.is_none() {
                self.show_settings = false;
            }
        }
    }

    fn settings_scroll_ui(&mut self, ui: &mut egui::Ui, actions: &mut SettingsActions) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.settings_content(ui, actions);
            });
    }

    fn settings_content(&mut self, ui: &mut egui::Ui, actions: &mut SettingsActions) {
        ui.label(egui::RichText::new("このアプリ").strong());
        egui::Grid::new("settings-about")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("バージョン");
                ui.label(format!("v{}", crate::VERSION));
                ui.end_row();
                ui.label("ログファイル");
                if let Some(path) = &self.log_path {
                    ui.horizontal(|ui| {
                        ui.monospace(path.display().to_string());
                        if ui.button("開く").clicked() {
                            actions.open_log = true;
                        }
                    });
                } else {
                    ui.label("初期化できませんでした");
                }
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("設定ファイル").strong());
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.config_path_draft);
            if ui.button("読み込む").clicked() {
                actions.load = true;
            }
            if ui.button("OS のアプリで開く").clicked() {
                actions.open_config = true;
            }
        });
        ui.label("保存もこのパスへ書き込みます。空欄の場合は現在の設定ファイルを使います。");
        ui.label("ファイルが無い場合は、現在の設定を保存してから開きます。");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("接続").strong());
        egui::Grid::new("settings-connection")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("チャンネル");
                egui::ComboBox::from_id_salt("channel")
                    .selected_text(&self.config.channel_id)
                    .show_ui(ui, |ui| {
                        for channel in CHANNELS {
                            ui.selectable_value(
                                &mut self.config.channel_id,
                                channel.to_owned(),
                                channel,
                            );
                        }
                    });
                ui.end_row();
                ui.label("OBS ホスト");
                ui.text_edit_singleline(&mut self.config.obs.host);
                ui.end_row();
                ui.label("OBS ポート");
                ui.add(egui::DragValue::new(&mut self.config.obs.port).range(1..=65535));
                ui.end_row();
                ui.label("OBS パスワード");
                ui.add(egui::TextEdit::singleline(&mut self.config.obs.password).password(true));
                ui.end_row();
            });
        ui.add_space(4.0);
        ui.label("publisher ホスト（上から順に試し、失敗したら次へ切り替える）");
        ui.add(
            egui::TextEdit::multiline(&mut self.publisher_hosts_draft)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("publisher.example.test"),
        );
        ui.label(
            "1 行に 1 つ。ポートを省略するとチャンネルごとの既定値を使います。チャンネルは /live/<channel> として自動で付きます。",
        );
        ui.label("既定ポート: uni 1935、1A 1936、kaikan 1937。");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("fallback 登録").strong());
        egui::Grid::new("settings-stream-credentials")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("ベース URL");
                ui.text_edit_singleline(&mut self.config.stream_credentials.base_url);
                ui.end_row();
                ui.label("STREAM_INGEST_TOKEN");
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.stream_credentials.ingest_token)
                        .password(true),
                );
                ui.end_row();
                ui.label("metric");
                ui.add(
                    egui::DragValue::new(&mut self.config.stream_credentials.metric)
                        .range(0..=i32::MAX as u32),
                );
                ui.end_row();
            });
        ui.label("空欄なら登録しません。鍵 URI はベース URL とチャンネル名から自動で決まります。");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("ImageFlux").strong());
        egui::Grid::new("settings-imageflux")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("API トークン");
                ui.add(egui::TextEdit::singleline(&mut self.config.imageflux.token).password(true));
                ui.end_row();
                ui.label("Webhook URL");
                ui.text_edit_singleline(&mut self.config.imageflux.event_webhook_url);
                ui.end_row();
            });
        ui.add_space(4.0);
        ui.label("HLS 出力（whip-script の設定を取り込んだ値。ここでは参照のみ）");
        egui::Grid::new("settings-hls")
            .num_columns(5)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label("解像度");
                ui.label("FPS");
                ui.label("映像 bps");
                ui.label("音声 bps");
                ui.label("セグメント秒");
                ui.end_row();
                for rendition in &self.config.imageflux.hls {
                    ui.label(format!(
                        "{}x{}",
                        rendition.video.width, rendition.video.height
                    ));
                    ui.label(rendition.video.fps.to_string());
                    ui.label(rendition.video.bps.to_string());
                    ui.label(rendition.audio.bps.to_string());
                    ui.label(rendition.duration_seconds.to_string());
                    ui.end_row();
                }
            });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("GStreamer").strong());
        ui.horizontal(|ui| {
            ui.label("実行ファイル");
            ui.text_edit_singleline(&mut self.gstreamer_path_draft);
        });
        ui.label("空欄なら PATH、TOMYOU_GST_LAUNCH、標準的な場所から自動検出します。");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("保存").clicked() {
                actions.save = true;
            }
            if ui.button("再読込").clicked() {
                actions.load = true;
            }
            if ui.button("閉じる").clicked() {
                actions.close = true;
            }
        });
        ui.add_space(4.0);
        ui.label("配信中は接続先の変更を反映しません。停止してから保存してください。");
    }

    fn license_window(&mut self, ctx: &egui::Context) {
        if !self.show_license {
            return;
        }
        let mut open = self.show_license;
        egui::Window::new("ライセンス")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label(format!("tomyou v{}", crate::VERSION));
                ui.label("Apache License 2.0");
                ui.separator();
                ui.label("同梱・使用するソフトウェア");
                ui.label("egui / eframe: MIT OR Apache-2.0");
                ui.label("shiguredo_rtmp: Apache-2.0");
                ui.label("obws: MIT");
                ui.label("GStreamer: LGPL。rswebrtc: MPL-2.0。配布物の LICENSE を参照");
                ui.separator();
                ui.label(
                    "ImageFlux の API トークン、STREAM_INGEST_TOKEN、OBS のパスワードは設定ファイルへ保存します。",
                );
                ui.label("秘密情報を含むため、設定ファイルの権限と配布先に注意してください。");
                ui.label("ログと画面には秘密情報を表示しません。");
            });
        self.show_license = open;
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        // Windows
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\YuGothM.ttc",
        // macOS
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansJP-Regular.otf",
    ];
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "system-japanese".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "system-japanese".to_owned());
        }
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.push("system-japanese".to_owned());
        }
        ctx.set_fonts(fonts);
        return;
    }
    warn!("Japanese system font was not found; GUI text may not render correctly");
}
