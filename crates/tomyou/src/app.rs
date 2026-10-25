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

    fn color(self) -> egui::Color32 {
        match self {
            Self::Idle => egui::Color32::GRAY,
            Self::Starting | Self::Stopping => egui::Color32::YELLOW,
            Self::Streaming => egui::Color32::GREEN,
            Self::Error => egui::Color32::RED,
        }
    }
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
    show_settings: bool,
    show_license: bool,
    gstreamer_path_draft: String,
    obs_snapshot: Option<StreamSnapshot>,
}

impl TomyouApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: PathBuf,
        runtime: tokio::runtime::Runtime,
        startup_error: Option<String>,
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
            show_settings: false,
            show_license: false,
            gstreamer_path_draft,
            obs_snapshot: None,
        }
    }

    fn log(&mut self, message: impl Into<String>) {
        let message = message.into();
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
        self.publisher_connected = false;
        self.imageflux_output = false;
        self.edamame_output = false;
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
                self.log("OBS からの RTMP 入力を受け付けました");
            }
            SessionEvent::PublisherDisconnected => {
                self.publisher_connected = false;
                self.log("OBS からの RTMP 入力が切れました");
            }
            SessionEvent::PlayerConnected => {
                self.log("ローカルの中継クライアントが接続しました");
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
            SessionEvent::OutputStarted(name) => {
                if name == "Edamame" {
                    self.edamame_output = true;
                } else {
                    self.imageflux_output = true;
                }
                self.log(format!("{name} への送信を開始しました"));
            }
            SessionEvent::OutputExited(name, message) => {
                if name == "Edamame" {
                    self.edamame_output = false;
                } else {
                    self.imageflux_output = false;
                }
                self.log(format!("{name} の送信が終了: {message}"));
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
                self.fallback_registered = false;
                self.fallback_error = None;
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
                ui.heading("tomyou");
                ui.separator();
                ui.label(format!("チャンネル: {}", self.config.channel_id));
                ui.separator();
                ui.label(
                    egui::RichText::new(self.status.label())
                        .color(self.status.color())
                        .strong(),
                );
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
                ui.colored_label(egui::Color32::RED, format!("起動時の設定: {error}"));
                ui.add_space(4.0);
            }
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
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
                        ui.label("チャンネル");
                        ui.label(&self.config.channel_id);
                        ui.end_row();
                    });
            });
            ui.add_space(8.0);
            ui.columns(2, |columns| {
                columns[0].group(|ui| {
                    ui.label(egui::RichText::new("Edamame（主系）").strong());
                    ui.label(if self.edamame_output {
                        "送信中"
                    } else if self.session.is_some() {
                        "接続待ち / 再接続中"
                    } else {
                        "停止"
                    });
                    ui.label(format!("送信先: {}", self.config.publisher.rtmp_url));
                });
                columns[1].group(|ui| {
                    ui.label(egui::RichText::new("ImageFlux（予備系）").strong());
                    ui.label(if self.imageflux_output {
                        "WHIP 送信中"
                    } else if self.session.is_some() {
                        "接続待ち / 再接続中"
                    } else {
                        "停止"
                    });
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
                    let (fallback_status, fallback_color) =
                        if !self.config.stream_credentials.enabled() {
                            ("未設定", egui::Color32::GRAY)
                        } else if self.fallback_error.is_some() {
                            ("失敗", egui::Color32::RED)
                        } else if self.fallback_registered {
                            ("登録済み", egui::Color32::GREEN)
                        } else if self.session.is_some() {
                            ("待機中", egui::Color32::YELLOW)
                        } else {
                            ("未登録", egui::Color32::GRAY)
                        };
                    ui.colored_label(
                        fallback_color,
                        format!("fallback 登録: {fallback_status}"),
                    );
                    if let Some(error) = &self.fallback_error {
                        ui.colored_label(egui::Color32::RED, error);
                    }
                });
            });
            ui.add_space(8.0);
            ui.label(
                "OBS の入力条件: H.264 / B フレームなし / AAC / キーフレーム間隔 2 秒を推奨",
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
                    ui.colored_label(egui::Color32::YELLOW, "OBS が再接続中です");
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
        let mut open = self.show_settings;
        let mut close_requested = false;
        let mut load_requested = false;
        let mut save_requested = false;
        let mut open_requested = false;
        egui::Window::new("設定")
            .open(&mut open)
            .default_width(720.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("設定ファイル").strong());
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.config_path_draft);
                    if ui.button("読み込む").clicked() {
                        load_requested = true;
                    }
                    if ui.button("OS のアプリで開く").clicked() {
                        open_requested = true;
                    }
                });
                ui.label(
                    "保存もこのパスへ書き込みます。空欄の場合は現在の設定ファイルを使います。",
                );
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
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.obs.password)
                                .password(true),
                        );
                        ui.end_row();
                        ui.label("publisher RTMP URL");
                        ui.text_edit_singleline(&mut self.config.publisher.rtmp_url);
                        ui.end_row();
                    });
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
                            egui::TextEdit::singleline(
                                &mut self.config.stream_credentials.ingest_token,
                            )
                            .password(true),
                        );
                        ui.end_row();
                    });
                ui.label(
                    "空欄なら登録しません。鍵 URI はベース URL とチャンネル名から自動で決まります。",
                );
                ui.add_space(8.0);
                ui.label(egui::RichText::new("ImageFlux").strong());
                egui::Grid::new("settings-imageflux")
                    .num_columns(2)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("API トークン");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.imageflux.token)
                                .password(true),
                        );
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
                        save_requested = true;
                    }
                    if ui.button("再読込").clicked() {
                        load_requested = true;
                    }
                    if ui.button("閉じる").clicked() {
                        close_requested = true;
                    }
                });
                ui.add_space(4.0);
                ui.label("配信中は接続先の変更を反映しません。停止してから保存してください。");
            });
        self.show_settings = open && !close_requested;
        if open_requested {
            self.open_config_file();
        }
        if load_requested {
            self.load_config_from_draft();
        }
        if save_requested {
            self.save_config();
            if self.error.is_none() {
                self.show_settings = false;
            }
        }
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
                ui.label("tomyou");
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
