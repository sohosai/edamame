//! OBS から受けた RTMP を、ローカルの再生クライアントへ分配する。
//!
//! ここでは映像の再エンコードやファイル保存は行わない。
//! OBS の publish を 1 接続だけ受け付け、複数の play 接続へ同じフレームを流す。

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use shiguredo_rtmp::{
    AudioFrame, MediaFrame, RtmpConnectionEvent, RtmpConnectionState, RtmpServerConnection,
    VideoFrame,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
    task::{JoinHandle, JoinSet},
};
use tracing::{info, warn};

/// 中継の状態変化
#[derive(Debug, Clone)]
pub enum RelayEvent {
    Listening(SocketAddr),
    PublisherConnected,
    PublisherDisconnected,
    PlayerConnected,
    PlayerDisconnected,
    Error(String),
}

/// 中継サーバのハンドル
pub struct RelayHandle {
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl RelayHandle {
    /// 受信を止める
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        // セッションが途中で失敗しても、待ち受けタスクを残さない
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// ローカル RTMP サーバを起動する
pub async fn start(
    bind: SocketAddr,
    app: impl Into<String>,
    stream: impl Into<String>,
    events: mpsc::UnboundedSender<RelayEvent>,
) -> Result<RelayHandle> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("RTMP の待ち受けを開始できません: {bind}"))?;
    let local = listener
        .local_addr()
        .context("RTMP の待ち受けアドレスを取得できません")?;
    let expected_app = app.into();
    let expected_stream = stream.into();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_task = Arc::clone(&stop);
    let shared = Arc::new(Mutex::new(HashMap::<String, StreamState>::new()));

    let task = tokio::spawn(async move {
        let _ = events.send(RelayEvent::Listening(local));
        info!("RTMP relay listening on {local}");
        let mut connections = JoinSet::new();
        let mut next_client_id = 1_usize;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if stop_for_task.load(Ordering::Relaxed) {
                        break;
                    }
                    while let Some(result) = connections.try_join_next() {
                        if let Err(error) = result {
                            let _ = events.send(RelayEvent::Error(format!("RTMP connection task failed: {error}")));
                        }
                    }
                }
                accepted = listener.accept() => {
                    let (socket, peer) = match accepted {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = events.send(RelayEvent::Error(format!("RTMP accept failed: {error}")));
                            continue;
                        }
                    };
                    info!("RTMP connection from {peer}");
                    let handler = ClientConnectionHandler::new(
                        next_client_id,
                        socket,
                        Arc::clone(&shared),
                        expected_app.clone(),
                        expected_stream.clone(),
                        events.clone(),
                    );
                    next_client_id += 1;
                    connections.spawn(async move {
                        if let Err(error) = handler.run().await {
                            warn!("RTMP connection from {peer} ended with error: {error}");
                        }
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        info!("RTMP relay stopped");
    });

    Ok(RelayHandle { stop, task })
}

struct ClientConnectionHandler {
    client_id: usize,
    stream: TcpStream,
    conn: RtmpServerConnection,
    recv_buf: [u8; 8192],
    shared: Arc<Mutex<HashMap<String, StreamState>>>,
    stream_id: String,
    is_publisher: bool,
    media_tx: mpsc::Sender<MediaFrame>,
    media_rx: mpsc::Receiver<MediaFrame>,
    events: mpsc::UnboundedSender<RelayEvent>,
    expected_app: String,
    expected_stream: String,
}

impl ClientConnectionHandler {
    fn new(
        client_id: usize,
        stream: TcpStream,
        shared: Arc<Mutex<HashMap<String, StreamState>>>,
        expected_app: String,
        expected_stream: String,
        events: mpsc::UnboundedSender<RelayEvent>,
    ) -> Self {
        let (media_tx, media_rx) = mpsc::channel(256);
        Self {
            client_id,
            stream,
            conn: RtmpServerConnection::new(),
            recv_buf: [0; 8192],
            shared,
            stream_id: String::new(),
            is_publisher: false,
            media_tx,
            media_rx,
            events,
            expected_app,
            expected_stream,
        }
    }

    async fn run(mut self) -> Result<()> {
        let result = loop {
            match self.run_one().await {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        self.cleanup().await;
        result
    }

    async fn run_one(&mut self) -> Result<bool> {
        loop {
            while let Some(event) = self.conn.next_event() {
                self.process_event(event).await?;
            }
            // ACK の判定は送信完了時にも変わるため、送信待ちフレームを取り出す前に再確認する。
            if self.conn.state() == RtmpConnectionState::Disconnecting {
                return Ok(false);
            }
            if self.conn.send_buf().is_empty() {
                break;
            }
            let data = self.conn.send_buf();
            self.stream
                .write_all(data)
                .await
                .context("RTMP の送信に失敗しました")?;
            let len = data.len();
            self.conn.advance_send_buf(len);
        }

        tokio::select! {
            Some(frame) = self.media_rx.recv() => {
                self.process_media_frame(frame)?;
            }
            result = self.stream.read(&mut self.recv_buf) => {
                match result.context("RTMP の受信に失敗しました")? {
                    0 => return Ok(false),
                    n => self.conn.feed_recv_buf(&self.recv_buf[..n])?,
                }
            }
        }
        Ok(true)
    }

    fn process_media_frame(&mut self, frame: MediaFrame) -> Result<()> {
        match frame {
            MediaFrame::Audio(frame) => self.conn.send_audio(frame)?,
            MediaFrame::Video(frame) => self.conn.send_video(frame)?,
        }
        Ok(())
    }

    async fn process_event(&mut self, event: RtmpConnectionEvent) -> Result<()> {
        match event {
            RtmpConnectionEvent::PublishRequested {
                app, stream_name, ..
            } => {
                let stream_id = format!("{app}/{stream_name}");
                self.handle_publish_requested(stream_id).await?;
            }
            RtmpConnectionEvent::PlayRequested {
                app, stream_name, ..
            } => {
                let stream_id = format!("{app}/{stream_name}");
                self.handle_play_requested(stream_id).await?;
            }
            RtmpConnectionEvent::AudioReceived(frame) => {
                self.handle_audio_received(frame).await?;
            }
            RtmpConnectionEvent::VideoReceived(frame) => {
                self.handle_video_received(frame).await?;
            }
            RtmpConnectionEvent::DisconnectedByPeer { reason } => {
                info!("RTMP peer disconnected: {reason}");
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_publish_requested(&mut self, stream_id: String) -> Result<()> {
        if stream_id != format!("{}/{}", self.expected_app, self.expected_stream) {
            let reason = format!("unexpected RTMP stream: {stream_id}");
            self.conn.reject(&reason)?;
            bail!(reason);
        }
        let mut streams = self.shared.lock().await;
        if streams
            .get(&stream_id)
            .is_some_and(|state| state.publisher_id.is_some())
        {
            let reason = format!("stream is already published: {stream_id}");
            self.conn.reject(&reason)?;
            bail!(reason);
        }
        self.conn.accept()?;
        let state = streams.entry(stream_id.clone()).or_default();
        state.publisher_id = Some(self.client_id);
        state.video_sequence_header = None;
        state.audio_sequence_header = None;
        self.stream_id = stream_id;
        self.is_publisher = true;
        let _ = self.events.send(RelayEvent::PublisherConnected);
        info!("RTMP publisher connected");
        Ok(())
    }

    async fn handle_play_requested(&mut self, stream_id: String) -> Result<()> {
        if stream_id != format!("{}/{}", self.expected_app, self.expected_stream) {
            let reason = format!("unexpected RTMP stream: {stream_id}");
            self.conn.reject(&reason)?;
            bail!(reason);
        }
        self.conn.accept()?;
        let mut streams = self.shared.lock().await;
        let state = streams.entry(stream_id.clone()).or_default();
        state.players.insert(
            self.client_id,
            PlayerState {
                tx: self.media_tx.clone(),
                sent_video_header: false,
                sent_audio_header: false,
                waiting_keyframe: true,
            },
        );
        if let Some(header) = state.video_sequence_header.clone() {
            let player = state
                .players
                .get_mut(&self.client_id)
                .expect("player exists");
            let _ = player.tx.try_send(MediaFrame::Video(header));
            player.sent_video_header = true;
        }
        if let Some(header) = state.audio_sequence_header.clone() {
            let player = state
                .players
                .get_mut(&self.client_id)
                .expect("player exists");
            let _ = player.tx.try_send(MediaFrame::Audio(header));
            player.sent_audio_header = true;
        }
        self.stream_id = stream_id;
        let _ = self.events.send(RelayEvent::PlayerConnected);
        info!("RTMP player connected");
        Ok(())
    }

    async fn handle_audio_received(&mut self, frame: AudioFrame) -> Result<()> {
        if !self.is_publisher {
            return Ok(());
        }
        let mut streams = self.shared.lock().await;
        let Some(state) = streams.get_mut(&self.stream_id) else {
            return Ok(());
        };
        if frame.is_aac_sequence_header {
            state.audio_sequence_header = Some(frame.clone());
        }
        for player in state.players.values_mut() {
            if !player.sent_audio_header
                && let Some(header) = state.audio_sequence_header.clone()
            {
                let _ = player.tx.try_send(MediaFrame::Audio(header));
                player.sent_audio_header = true;
            }
            let _ = player.tx.try_send(MediaFrame::Audio(frame.clone()));
        }
        Ok(())
    }

    async fn handle_video_received(&mut self, frame: VideoFrame) -> Result<()> {
        if !self.is_publisher {
            return Ok(());
        }
        let mut streams = self.shared.lock().await;
        let Some(state) = streams.get_mut(&self.stream_id) else {
            return Ok(());
        };
        let is_sequence_header =
            frame.avc_packet_type == Some(shiguredo_rtmp::AvcPacketType::SequenceHeader);
        if is_sequence_header {
            state.video_sequence_header = Some(frame.clone());
        }
        for player in state.players.values_mut() {
            if is_sequence_header {
                if !player.sent_video_header {
                    if player
                        .tx
                        .try_send(MediaFrame::Video(frame.clone()))
                        .is_err()
                    {
                        // 遅い再生側は次のキーフレームから再開する
                        player.waiting_keyframe = true;
                    }
                    player.sent_video_header = true;
                }
                continue;
            }
            if player.waiting_keyframe {
                if !frame.is_keyframe() {
                    continue;
                }
                if !player.sent_video_header {
                    if let Some(header) = state.video_sequence_header.clone() {
                        let _ = player.tx.try_send(MediaFrame::Video(header));
                        player.sent_video_header = true;
                    } else {
                        continue;
                    }
                }
                player.waiting_keyframe = false;
            }
            if player
                .tx
                .try_send(MediaFrame::Video(frame.clone()))
                .is_err()
            {
                // 遅い再生側は次のキーフレームから再開する
                player.waiting_keyframe = true;
            }
        }
        Ok(())
    }

    async fn cleanup(&mut self) {
        if self.stream_id.is_empty() {
            return;
        }
        let Ok(mut streams) =
            tokio::time::timeout(Duration::from_millis(200), self.shared.lock()).await
        else {
            return;
        };
        let Some(state) = streams.get_mut(&self.stream_id) else {
            return;
        };
        if self.is_publisher && state.publisher_id == Some(self.client_id) {
            state.publisher_id = None;
            state.video_sequence_header = None;
            state.audio_sequence_header = None;
            let _ = self.events.send(RelayEvent::PublisherDisconnected);
            info!("RTMP publisher disconnected");
        }
        if state.players.remove(&self.client_id).is_some() {
            let _ = self.events.send(RelayEvent::PlayerDisconnected);
            info!("RTMP player disconnected");
        }
    }
}

#[derive(Default)]
struct StreamState {
    publisher_id: Option<usize>,
    video_sequence_header: Option<VideoFrame>,
    audio_sequence_header: Option<AudioFrame>,
    players: HashMap<usize, PlayerState>,
}

struct PlayerState {
    tx: mpsc::Sender<MediaFrame>,
    sent_video_header: bool,
    sent_audio_header: bool,
    waiting_keyframe: bool,
}
