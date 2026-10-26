//! channel ごとのエンコード、セグメントの収集、暗号化、controller への PUT

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fmt::Write as _, io};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use reqwest::blocking::Client;

use crate::config::{PublisherSettings, RenditionMode, RenditionSettings};
use crate::credentials;
use crate::ffmpeg::{self, Ffmpeg, PLAYLIST_NAME};

/// 作業ディレクトリを走査する間隔
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 1 回の PUT の制限時間
const PUT_TIMEOUT: Duration = Duration::from_secs(10);
/// PUT を再試行するまでの待ち時間
const PUT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// ffmpeg を起動し直すときの待ち時間
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(10);
/// 不正な入力を拒否したあと、次の入力を待つまでの時間
const INPUT_RESTART_DELAY: Duration = Duration::from_secs(1);
/// 拒否時に、送信待ちのセットが処理されるのを待つ上限
const PENDING_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// コピーする rendition のセグメント長を比べるときの許容幅
const COPY_DURATION_TOLERANCE_MS: u64 = 100;
/// プレイリスト取得時の制限時間と試行回数
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(5);
const PLAYLIST_ATTEMPTS: u32 = 3;
const PLAYLIST_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// 1 つの channel を動かすために必要な設定
pub struct ChannelContext {
    pub settings: Arc<PublisherSettings>,
    /// 対象の入力が `settings.inputs` の何番目にあるかを表す
    pub input_index: usize,
    pub key: [u8; 16],
    pub work_dir: PathBuf,
}

impl ChannelContext {
    #[must_use]
    pub fn channel_id(&self) -> &str {
        &self.input().channel_id
    }

    #[must_use]
    pub fn input_url(&self) -> &str {
        &self.input().input_url
    }

    #[must_use]
    pub fn renditions(&self) -> &[RenditionSettings] {
        &self.input().renditions
    }

    fn input(&self) -> &crate::config::InputSettings {
        &self.settings.inputs[self.input_index]
    }

    fn max_age(&self) -> Duration {
        Duration::from_millis(self.settings.segment_duration_ms * self.settings.backlog_limit)
    }
}

/// controller へ送る 1 セット分のセグメント
struct PendingSet {
    /// publisher が数える通し番号。ログだけで使う
    index: u64,
    /// 全 rendition で同じ値を使うセグメント識別子
    identifier: String,
    files: Vec<SegmentFile>,
    enqueued_at: Instant,
}

struct SegmentFile {
    quality: String,
    path: PathBuf,
}

/// 送信中のセットをどう扱うか
enum PublishError {
    /// 再試行では解消しないエラーのため、いったん channel を止める
    Fatal(String),
    /// 期限を過ぎたか停止したので、このセットは捨てる
    Abandoned(String),
}

/// セグメントの収集で起きた問題
enum SegmentError {
    /// 入力が設定に合わないので、セットを捨てて入力を待ち直す
    Rejected(String),
    /// ローカルの障害なので、channel を起動し直す
    Fatal(String),
}

/// 16 byte の乱数から暗号鍵を作る
pub fn random_key() -> Result<[u8; 16], String> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).map_err(|error| format!("failed to generate a key: {error}"))?;
    Ok(key)
}

/// 鍵ファイルを読み、内容がちょうど 16 byte でなければエラーにする。
/// ファイルがなければ鍵を生成し、権限 0600 を指定して新規ファイルに保存する。
/// 既存のファイルは上書きしない
pub fn load_or_create_key(path: &Path) -> Result<[u8; 16], String> {
    match fs::read(path) {
        Ok(bytes) => {
            let key: [u8; 16] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| format!("key file must contain 16 bytes: {}", path.display()))?;
            Ok(key)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let key = random_key()?;
            write_key_file(path, &key)?;
            Ok(key)
        }
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

/// 鍵を 16 byte の生データとして保存する
fn write_key_file(path: &Path, key: &[u8; 16]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // 鍵は他の利用者から読めないようにする
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    file.write_all(key)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(())
}

/// バイト列を hex 文字列にする
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // String への書き込みは失敗しない
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 16 byte の乱数から IV を作る
pub fn random_iv() -> Result<[u8; 16], String> {
    let mut iv = [0u8; 16];
    getrandom::fill(&mut iv).map_err(|error| format!("failed to generate an iv: {error}"))?;
    Ok(iv)
}

/// セグメントを AES-128-CBC と PKCS7 で暗号化する
///
/// 再生側が `#EXT-X-KEY` に同じ IV を書けば復号できる。
#[must_use]
pub fn encrypt_segment(key: &[u8; 16], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// controller のセグメントファイルの URL を組み立てる
#[must_use]
pub fn segment_url(
    controller_url: &str,
    channel_id: &str,
    quality: &str,
    identifier: &str,
) -> String {
    format!("{controller_url}/segments/{channel_id}/{quality}/{identifier}")
}

/// PUT に付けるクエリを組み立てる
#[must_use]
pub fn segment_query(
    context: &ChannelContext,
    rendition: &RenditionSettings,
    iv_hex: &str,
) -> Vec<(&'static str, String)> {
    let mut query = vec![
        (
            "duration_ms",
            context.settings.segment_duration_ms.to_string(),
        ),
        ("bandwidth", rendition.bandwidth.to_string()),
        ("resolution", rendition.resolution.text()),
    ];
    if let Some(key_uri) = context.input().key_uri.as_deref() {
        // 暗号化に使った IV を controller に伝え、プレイリストの #EXT-X-KEY に反映させる
        query.push(("key_uri", key_uri.to_owned()));
        query.push(("iv", iv_hex.to_owned()));
    }
    query
}

/// channel ごとのエンコードと送信を始める
pub fn spawn_channel(
    context: ChannelContext,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let context = Arc::new(context);
        let registered = Arc::new(AtomicBool::new(
            context.settings.stream_credentials_url.is_none(),
        ));

        let registration = {
            let context = Arc::clone(&context);
            let stop = Arc::clone(&stop);
            let failed = Arc::clone(&failed);
            let registered = Arc::clone(&registered);
            thread::spawn(move || {
                register_stream_credentials(&context, &stop, &failed, &registered)
            })
        };

        // PUT の設定誤りなどでチャンネルが止まっても、待ち受けを再開して次の入力を待つ。
        // 止まったままにすると、送信側が設定を直して接続し直しても復帰できない。
        // run の番号は作業ディレクトリに残っている番号の続きから使い、
        // 前の実行が残したセグメントを新しい実行が読み直さないようにする
        let run_counter = Arc::new(AtomicU64::new(last_run_number(&context.work_dir)));
        let mut restart_delay = RESTART_MIN;
        while !stop.load(Ordering::Relaxed) {
            let channel_stop = Arc::new(AtomicBool::new(false));
            let start_sequence = starting_sequence(&context);
            let published = Arc::new(AtomicU64::new(start_sequence));
            // 拒否時に、送信待ちのセットが処理し終わるまで次の ffmpeg を起動しないために数える
            let pending = Arc::new(AtomicU64::new(0));
            let (sender, receiver) = mpsc::channel();

            let encoder = {
                let context = Arc::clone(&context);
                let stop = Arc::clone(&stop);
                let channel_stop = Arc::clone(&channel_stop);
                let published = Arc::clone(&published);
                let pending = Arc::clone(&pending);
                let run_counter = Arc::clone(&run_counter);
                thread::spawn(move || {
                    if let Err(message) = run_encoder(
                        &context,
                        &sender,
                        &stop,
                        &channel_stop,
                        &published,
                        &pending,
                        &run_counter,
                    ) {
                        println!("channel={} stopped: {message}", context.channel_id());
                    }
                })
            };
            let publisher = {
                let context = Arc::clone(&context);
                let stop = Arc::clone(&stop);
                let channel_stop = Arc::clone(&channel_stop);
                let published = Arc::clone(&published);
                let pending = Arc::clone(&pending);
                thread::spawn(move || {
                    if let Err(message) = run_publisher(
                        &context,
                        &receiver,
                        &stop,
                        &channel_stop,
                        &published,
                        &pending,
                    ) {
                        println!("channel={} stopped: {message}", context.channel_id());
                    }
                })
            };

            let _ = encoder.join();
            // エンコードが終わったら送信側も止め、残りのセグメントは捨てる
            channel_stop.store(true, Ordering::Relaxed);
            let _ = publisher.join();

            if stop.load(Ordering::Relaxed) {
                break;
            }

            // セグメントを 1 つでも配信できた場合はすぐ再開し、
            // 一度も配信できないまま止まった場合は間隔を広げる
            if published.load(Ordering::Relaxed) > start_sequence {
                restart_delay = RESTART_MIN;
            } else {
                restart_delay = (restart_delay * 2).min(RESTART_MAX);
            }
            println!(
                "channel={} restarting the channel after {} ms",
                context.channel_id(),
                restart_delay.as_millis()
            );
            // channel は既に止まっているため、プロセスの停止要求だけを確認しながら待つ
            let deadline = Instant::now() + restart_delay;
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                thread::sleep(POLL_INTERVAL);
            }
        }

        let _ = registration.join();
        // 登録できないまま終わった場合も、運用者が気づけるように失敗として扱う
        if !registered.load(Ordering::Relaxed) {
            println!(
                "channel={} stream credentials were not registered",
                context.channel_id()
            );
            failed.store(true, Ordering::Relaxed);
        }
    })
}

/// channel の作業ディレクトリに残っている run の最大番号。
///
/// 前のプロセスが残した run には、止まる原因になったセグメントが残っている。
/// 番号を続きから使わないと、新しい実行がそれを読み直して同じ理由で止まる。
fn last_run_number(work_dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(work_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("run-")?.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// 鍵と配信 URL を live2025-server へ登録する
fn register_stream_credentials(
    context: &ChannelContext,
    stop: &AtomicBool,
    failed: &AtomicBool,
    registered: &AtomicBool,
) {
    let (Some(base_url), Some(token)) = (
        context.settings.stream_credentials_url.as_deref(),
        context.settings.stream_ingest_token.as_deref(),
    ) else {
        return;
    };
    let Some(live_url) = context.input().live_url.as_deref() else {
        return;
    };
    let client = match credentials::client() {
        Ok(client) => client,
        Err(message) => {
            println!(
                "channel={} failed to register stream credentials: {message}",
                context.channel_id()
            );
            failed.store(true, Ordering::Relaxed);
            return;
        }
    };
    let url = credentials::credentials_url(base_url);
    let body =
        credentials::credentials_body(context.channel_id(), live_url, &hex_encode(&context.key));
    match credentials::register_until_success(
        &client,
        &url,
        token,
        &body,
        context.channel_id(),
        stop,
    ) {
        Ok(()) => {
            registered.store(true, Ordering::Relaxed);
            println!(
                "channel={} registered stream credentials",
                context.channel_id()
            );
        }
        Err(message) => {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            println!(
                "channel={} failed to register stream credentials: {message}",
                context.channel_id()
            );
            failed.store(true, Ordering::Relaxed);
        }
    }
}

/// 起動時に controller のプレイリストから次のセグメント番号を求める。
/// プレイリストを取得できない場合は 0 から始める
fn starting_sequence(context: &ChannelContext) -> u64 {
    let client = match Client::builder().timeout(PLAYLIST_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            println!(
                "channel={} failed to build the HTTP client ({error}); starting from 0",
                context.channel_id()
            );
            return 0;
        }
    };
    match fetch_next_sequence(
        &client,
        &context.settings.controller_url,
        context.channel_id(),
    ) {
        Ok(sequence) => {
            println!(
                "channel={} starting from sequence {sequence}",
                context.channel_id()
            );
            sequence
        }
        Err(message) => {
            println!(
                "channel={} failed to read the playlist ({message}); starting from 0",
                context.channel_id()
            );
            0
        }
    }
}

/// channel 全体の次のセグメント番号を返す
fn fetch_next_sequence(
    client: &Client,
    controller_url: &str,
    channel_id: &str,
) -> Result<u64, String> {
    let master_url = format!("{controller_url}/channels/{channel_id}/master.m3u8");
    let master = match get_playlist(client, &master_url, false)? {
        Some(body) => body,
        // channel がまだ無い場合だけ 0 から始める
        None => return Ok(0),
    };

    let mut next = 0;
    for quality in parse_master_qualities(&master) {
        let media_url = format!("{controller_url}/channels/{channel_id}/{quality}.m3u8");
        let body = get_playlist(client, &media_url, true)?
            .ok_or_else(|| format!("media playlist is not available: {media_url}"))?;
        for sequence in parse_playlist_sequences(&body) {
            next = next.max(sequence + 1);
        }
    }
    Ok(next)
}

/// プレイリストを取得する。
/// 再試行が有効な場合、422 と 503 では待ち時間をおいて取得を再試行する
fn get_playlist(client: &Client, url: &str, retry: bool) -> Result<Option<String>, String> {
    let attempts = if retry { PLAYLIST_ATTEMPTS } else { 1 };
    for attempt in 1..=attempts {
        match client.get(url).send() {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return response
                        .text()
                        .map(Some)
                        .map_err(|error| format!("failed to read {url}: {error}"));
                }
                if status.as_u16() == 404 {
                    return Ok(None);
                }
                if !matches!(status.as_u16(), 422 | 503) || attempt == attempts {
                    return Err(format!("{url} returned status {status}"));
                }
            }
            Err(error) => {
                if attempt == attempts {
                    return Err(format!("{url} failed: {error}"));
                }
            }
        }
        thread::sleep(PLAYLIST_RETRY_INTERVAL);
    }
    Err(format!("{url} is not available"))
}

/// マスタープレイリストに並ぶ quality を取り出す
fn parse_master_qualities(text: &str) -> Vec<String> {
    let mut qualities = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.ends_with(".m3u8") {
            continue;
        }
        let quality = line.trim_end_matches(".m3u8");
        if !quality.is_empty() {
            qualities.push(quality.to_owned());
        }
    }
    qualities
}

/// メディアプレイリストに並ぶセグメント番号を取り出す
fn parse_playlist_sequences(text: &str) -> Vec<u64> {
    let mut sequences = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.ends_with(".ts") {
            continue;
        }
        let name = line.rsplit('/').next().unwrap_or(line);
        if let Some(sequence) = name
            .strip_suffix(".ts")
            .and_then(|value| value.parse::<u64>().ok())
        {
            sequences.push(sequence);
        }
    }
    sequences
}

/// 1 つの channel の ffmpeg を起動し、完成したセグメントを送信側へ渡す
fn run_encoder(
    context: &ChannelContext,
    sender: &Sender<PendingSet>,
    stop: &AtomicBool,
    channel_stop: &AtomicBool,
    published: &AtomicU64,
    pending: &AtomicU64,
    run_counter: &AtomicU64,
) -> Result<(), String> {
    let mut restart_delay = RESTART_MIN;

    'restart_input: while !stop.load(Ordering::Relaxed) && !channel_stop.load(Ordering::Relaxed) {
        let run_number = run_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let run_dir = context.work_dir.join(format!("run-{run_number}"));
        for rendition in context.renditions() {
            fs::create_dir_all(run_dir.join(&rendition.quality))
                .map_err(|error| format!("failed to create the work directory: {error}"))?;
        }

        // 出力の時刻を、それまでに公開したセグメントの合計に合わせる
        let offset_ms = published.load(Ordering::Relaxed) * context.settings.segment_duration_ms;
        let args = ffmpeg::build_command(
            &context.settings,
            context.input_url(),
            context.renditions(),
            &run_dir,
            offset_ms,
        );
        println!(
            "channel={} starting ffmpeg run={run_number} offset_ms={offset_ms}",
            context.channel_id()
        );
        let mut process = Ffmpeg::spawn(&args, context.channel_id())?;
        let mut run = RunState::new(run_dir.clone());

        loop {
            if stop.load(Ordering::Relaxed) || channel_stop.load(Ordering::Relaxed) {
                process.stop();
                return Ok(());
            }
            if let Some(status) = process.try_wait()? {
                println!(
                    "channel={} ffmpeg exited status={status}",
                    context.channel_id()
                );
                break;
            }
            match collect_segments(context, &mut run, sender, pending) {
                Ok(()) => {}
                Err(SegmentError::Rejected(message)) => {
                    reject_input(
                        context,
                        &mut process,
                        &mut run,
                        &message,
                        pending,
                        stop,
                        channel_stop,
                    )?;
                    continue 'restart_input;
                }
                Err(SegmentError::Fatal(message)) => return Err(message),
            }
            thread::sleep(POLL_INTERVAL);
        }

        // 終了間際に完成したセグメントを集め、揃わなかったものは捨てる
        match collect_segments(context, &mut run, sender, pending) {
            Ok(()) => {}
            Err(SegmentError::Rejected(message)) => {
                reject_input(
                    context,
                    &mut process,
                    &mut run,
                    &message,
                    pending,
                    stop,
                    channel_stop,
                )?;
                continue 'restart_input;
            }
            Err(SegmentError::Fatal(message)) => return Err(message),
        }
        run.discard_incomplete(context)?;

        if run.produced == 0 {
            restart_delay = (restart_delay * 2).min(RESTART_MAX);
        } else {
            restart_delay = RESTART_MIN;
        }
        // 再起動の前に少し待ち、その間も停止要求を確認する
        sleep_interruptible(restart_delay, stop, channel_stop);
    }
    Ok(())
}

/// 不正な入力を拒否し、channel を維持したまま ffmpeg を起動し直せる状態にする
///
/// channel 全体を止めると、controller のプレイリスト読み直しとバックオフの間、
/// 送信側が再接続できなくなる。ffmpeg だけを止め、送信待ちのセットが
/// 処理し終わるのを待ってから次の入力を待つ。
fn reject_input(
    context: &ChannelContext,
    process: &mut Ffmpeg,
    run: &mut RunState,
    message: &str,
    pending: &AtomicU64,
    stop: &AtomicBool,
    channel_stop: &AtomicBool,
) -> Result<(), String> {
    println!(
        "channel={} rejected the input: {message}; restarting the input",
        context.channel_id()
    );
    process.stop();
    run.discard_incomplete(context)?;
    wait_for_pending_sets(pending, stop, channel_stop);
    sleep_interruptible(INPUT_RESTART_DELAY, stop, channel_stop);
    Ok(())
}

/// 送信側が送信待ちのセットを処理し終えるのを、上限付きで待つ
fn wait_for_pending_sets(pending: &AtomicU64, stop: &AtomicBool, channel_stop: &AtomicBool) {
    let deadline = Instant::now() + PENDING_DRAIN_TIMEOUT;
    while pending.load(Ordering::Relaxed) > 0
        && Instant::now() < deadline
        && !stop.load(Ordering::Relaxed)
        && !channel_stop.load(Ordering::Relaxed)
    {
        thread::sleep(Duration::from_millis(50));
    }
}

/// ffmpeg を 1 回起動して作られたセグメントの状態
struct RunState {
    run_dir: PathBuf,
    /// セグメント番号 -> quality -> ファイル
    files: BTreeMap<u64, BTreeMap<String, PathBuf>>,
    /// 送信側へ渡したセグメント番号
    sent: BTreeSet<u64>,
    /// 長さを確かめたセグメント番号
    checked: BTreeSet<u64>,
    /// 今回の ffmpeg 起動後に送信側へ渡したセット数
    produced: u64,
}

impl RunState {
    fn new(run_dir: PathBuf) -> Self {
        Self {
            run_dir,
            files: BTreeMap::new(),
            sent: BTreeSet::new(),
            checked: BTreeSet::new(),
            produced: 0,
        }
    }
}

/// 完成したセグメントを集めて送信側へ渡す
fn collect_segments(
    context: &ChannelContext,
    run: &mut RunState,
    sender: &Sender<PendingSet>,
    pending: &AtomicU64,
) -> Result<(), SegmentError> {
    check_copy_segment_durations(context, run).map_err(SegmentError::Fatal)?;
    read_files(context, run).map_err(SegmentError::Fatal)?;

    while let Some(index) = next_complete_index(context, run) {
        let files = context
            .renditions()
            .iter()
            .filter_map(|rendition| {
                run.files
                    .get(&index)?
                    .get(&rendition.quality)
                    .map(|path| SegmentFile {
                        quality: rendition.quality.clone(),
                        path: path.clone(),
                    })
            })
            .collect::<Vec<_>>();

        // 拒否したセットは送らずに削除し、残りの未完成セットは呼び出し側で捨てる
        if let Err(message) = validate_set(context, &files, run.produced == 0) {
            let _ = run.files.remove(&index);
            for file in &files {
                let _ = fs::remove_file(&file.path);
            }
            return Err(SegmentError::Rejected(message));
        }

        let identifier = new_identifier().map_err(SegmentError::Fatal)?;
        let _ = run.files.remove(&index);
        let set = PendingSet {
            index,
            identifier,
            files,
            enqueued_at: Instant::now(),
        };
        // 送信側が処理し終えたことを数えられるよう、送る前に増やす
        pending.fetch_add(1, Ordering::Relaxed);
        if sender.send(set).is_err() {
            pending.fetch_sub(1, Ordering::Relaxed);
            return Err(SegmentError::Fatal(
                "the publisher thread has stopped".to_owned(),
            ));
        }
        run.sent.insert(index);
        run.produced += 1;
    }
    Ok(())
}

/// コピーする rendition のセグメント長を、プレイリストから確かめる
///
/// HLS muxer は実行の先頭からセグメント長の倍数の位置で切るため、切り位置は
/// 「次のセグメントが現れたか」で分かる。入力のキーフレーム間隔がセグメント長を
/// 割り切らない場合、切れなかった分が次のセグメントに回り、長いセグメントと
/// 短いセグメントが混ざる。
fn check_copy_segment_durations(
    context: &ChannelContext,
    run: &mut RunState,
) -> Result<(), String> {
    for rendition in context.renditions() {
        if rendition.mode != RenditionMode::Copy {
            continue;
        }
        let path = run.run_dir.join(&rendition.quality).join(PLAYLIST_NAME);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let entries = ffmpeg::parse_segment_durations(&text)
            .iter()
            .filter_map(|(name, seconds)| parse_segment_index(name).map(|index| (index, *seconds)))
            .collect::<Vec<_>>();
        for (index, seconds) in &entries {
            if !entries.iter().any(|(next, _)| *next == index + 1) {
                continue;
            }
            if !run.checked.insert(*index) {
                continue;
            }
            let actual_ms = (seconds * 1000.0).round() as u64;
            if actual_ms.abs_diff(context.settings.segment_duration_ms) > COPY_DURATION_TOLERANCE_MS
            {
                println!(
                    "channel={} warning: copied segment index={index} quality={} is {actual_ms} ms \
                     but segment_duration_ms is {}; check the keyframe interval of the input",
                    context.channel_id(),
                    rendition.quality,
                    context.settings.segment_duration_ms
                );
            }
        }
    }
    Ok(())
}

/// 作業ディレクトリにあるセグメントファイルを探す
fn read_files(context: &ChannelContext, run: &mut RunState) -> Result<(), String> {
    for rendition in context.renditions() {
        let dir = run.run_dir.join(&rendition.quality);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("failed to read {}: {error}", dir.display()));
            }
        };
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read {}: {error}", dir.display()))?;
            let name = entry.file_name();
            let Some(index) = name.to_str().and_then(parse_segment_index) else {
                continue;
            };
            // 送信側へ渡したファイルは、送信後も publisher が削除するまでディスクに残る。
            // 再走査で見つかっても、送信側へ重複して渡さない
            if run.sent.contains(&index) {
                continue;
            }
            run.files
                .entry(index)
                .or_default()
                .insert(rendition.quality.clone(), entry.path());
        }
    }
    Ok(())
}

/// 全 rendition が揃った最小のセグメント番号を返す
fn next_complete_index(context: &ChannelContext, run: &RunState) -> Option<u64> {
    run.files.iter().find_map(|(index, qualities)| {
        let complete = context
            .renditions()
            .iter()
            .all(|rendition| qualities.contains_key(&rendition.quality));
        complete.then_some(*index)
    })
}

/// `12.ts` のような名前から番号を取り出す
fn parse_segment_index(name: &str) -> Option<u64> {
    name.strip_suffix(".ts")?.parse::<u64>().ok()
}

/// 完成したセットが入力の前提を満たすか確かめる
///
/// 音声はそのままコピーするため AAC に限る。copy の映像は H.264 で、
/// 設定した解像度と一致しなければならない。
fn validate_set(
    context: &ChannelContext,
    files: &[SegmentFile],
    first_set: bool,
) -> Result<(), String> {
    for file in files {
        let rendition = context
            .renditions()
            .iter()
            .find(|rendition| rendition.quality == file.quality)
            .ok_or_else(|| format!("unknown quality: {}", file.quality))?;
        let streams = ffmpeg::probe_streams(&file.path)?;

        // 音声はそのままコピーするので、aac 以外は再生できない
        for stream in streams.iter().filter(|stream| stream.codec_type == "audio") {
            if stream.codec_name != "aac" {
                return Err(format!(
                    "audio codec must be aac but is {}: quality {}",
                    stream.codec_name, file.quality
                ));
            }
        }

        if rendition.mode == RenditionMode::Copy {
            let video = streams
                .iter()
                .find(|stream| stream.codec_type == "video")
                .ok_or_else(|| format!("video stream is missing: quality {}", file.quality))?;
            if video.codec_name != "h264" {
                return Err(format!(
                    "copied video codec must be h264 but is {}: quality {}",
                    video.codec_name, file.quality
                ));
            }
            let resolution = rendition.resolution;
            if video.width != Some(resolution.width) || video.height != Some(resolution.height) {
                return Err(format!(
                    "copied video resolution must be {} but is {}x{}: quality {}",
                    resolution.text(),
                    video.width.unwrap_or(0),
                    video.height.unwrap_or(0),
                    file.quality
                ));
            }
            if first_set
                && let Some(field_order) = &video.field_order
                && field_order != "progressive"
                && field_order != "unknown"
            {
                println!(
                    "channel={} warning: copied video is {}: quality {}",
                    context.channel_id(),
                    field_order,
                    file.quality
                );
            }
        }
    }
    Ok(())
}

impl RunState {
    /// 揃わなかったセグメントを捨てる
    fn discard_incomplete(&mut self, context: &ChannelContext) -> Result<(), String> {
        for (index, qualities) in std::mem::take(&mut self.files) {
            if self.sent.contains(&index) {
                continue;
            }
            println!(
                "channel={} dropping incomplete segment index={index}",
                context.channel_id()
            );
            for path in qualities.values() {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}

/// 完成したセットを controller へ送る
fn run_publisher(
    context: &ChannelContext,
    receiver: &Receiver<PendingSet>,
    stop: &AtomicBool,
    channel_stop: &AtomicBool,
    published: &AtomicU64,
    pending: &AtomicU64,
) -> Result<(), String> {
    let client = Client::builder()
        .timeout(PUT_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build the HTTP client: {error}"))?;
    let max_age = context.max_age();
    let mut queue: VecDeque<PendingSet> = VecDeque::new();

    loop {
        if stop.load(Ordering::Relaxed) || channel_stop.load(Ordering::Relaxed) {
            break;
        }
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(set) => queue.push_back(set),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if queue.is_empty() {
                    break;
                }
            }
        }

        // 期限を過ぎたセットは、配信が遅れる原因になるので捨てる
        while queue
            .front()
            .is_some_and(|set| set.enqueued_at.elapsed() > max_age)
        {
            let dropped = queue.pop_front().expect("the front was checked");
            println!(
                "channel={} dropping expired segment index={}",
                context.channel_id(),
                dropped.index
            );
            delete_files(&dropped);
            pending.fetch_sub(1, Ordering::Relaxed);
        }

        let Some(set) = queue.pop_front() else {
            continue;
        };
        match publish_set(&client, context, &set, stop, channel_stop, max_age) {
            Ok(()) => {
                published.fetch_add(1, Ordering::Relaxed);
                pending.fetch_sub(1, Ordering::Relaxed);
            }
            Err(PublishError::Abandoned(reason)) => {
                println!(
                    "channel={} abandoning segment index={}: {reason}",
                    context.channel_id(),
                    set.index
                );
                delete_files(&set);
                pending.fetch_sub(1, Ordering::Relaxed);
            }
            Err(PublishError::Fatal(message)) => {
                delete_files(&set);
                pending.fetch_sub(1, Ordering::Relaxed);
                // 送信側を止め、エンコード側も終わらせる
                channel_stop.store(true, Ordering::Relaxed);
                return Err(message);
            }
        }
    }

    for set in queue {
        delete_files(&set);
        pending.fetch_sub(1, Ordering::Relaxed);
    }
    Ok(())
}

/// 1 セット分のセグメントを暗号化して PUT する
fn publish_set(
    client: &Client,
    context: &ChannelContext,
    set: &PendingSet,
    stop: &AtomicBool,
    channel_stop: &AtomicBool,
    max_age: Duration,
) -> Result<(), PublishError> {
    for file in &set.files {
        let Some(rendition) = context
            .renditions()
            .iter()
            .find(|rendition| rendition.quality == file.quality)
        else {
            return Err(PublishError::Fatal(format!(
                "unknown quality: {}",
                file.quality
            )));
        };
        let plaintext = fs::read(&file.path).map_err(|error| {
            PublishError::Fatal(format!("failed to read {}: {error}", file.path.display()))
        })?;
        // 暗号鍵の URI を指定しない場合は controller を介して IV を伝えられないため、全ゼロ固定にする
        let iv = if context.input().key_uri.is_some() {
            random_iv().map_err(PublishError::Fatal)?
        } else {
            [0u8; 16]
        };
        let encrypted = encrypt_segment(&context.key, &iv, &plaintext);
        let iv_hex = hex_encode(&iv);
        let url = segment_url(
            &context.settings.controller_url,
            context.channel_id(),
            &rendition.quality,
            &set.identifier,
        );

        loop {
            if stop.load(Ordering::Relaxed) || channel_stop.load(Ordering::Relaxed) {
                return Err(PublishError::Abandoned("stopping".to_owned()));
            }
            if set.enqueued_at.elapsed() > max_age {
                return Err(PublishError::Abandoned("expired".to_owned()));
            }
            match put_segment(client, context, rendition, &url, &iv_hex, encrypted.clone()) {
                Ok(sequence) => {
                    println!(
                        "channel={} published index={} quality={} identifier={} sequence={sequence}",
                        context.channel_id(),
                        set.index,
                        file.quality,
                        set.identifier
                    );
                    break;
                }
                Err(PutError::Retryable(message)) => {
                    println!(
                        "channel={} retrying index={} quality={}: {message}",
                        context.channel_id(),
                        set.index,
                        file.quality
                    );
                    sleep_interruptible(PUT_RETRY_INTERVAL, stop, channel_stop);
                }
                Err(PutError::Fatal(message)) => {
                    return Err(PublishError::Fatal(format!(
                        "failed to publish index={} quality={}: {message}",
                        set.index, file.quality
                    )));
                }
            }
        }
        let _ = fs::remove_file(&file.path);
    }
    Ok(())
}

/// PUT の結果
enum PutError {
    /// 時間をおいて同じセグメントを送り直す
    Retryable(String),
    /// 設定か入力を直さないと送信に成功しない
    Fatal(String),
}

/// セグメントファイルを 1 回 PUT する
fn put_segment(
    client: &Client,
    context: &ChannelContext,
    rendition: &RenditionSettings,
    url: &str,
    iv_hex: &str,
    body: Vec<u8>,
) -> Result<u64, PutError> {
    let query = segment_query(context, rendition, iv_hex);
    let mut request = client
        .put(url)
        .query(&query)
        .header("Content-Type", "application/octet-stream")
        .body(body);
    if let Some(token) = &context.settings.auth_token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => return Err(PutError::Retryable(format!("request failed: {error}"))),
    };
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status.is_success() {
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| PutError::Retryable(format!("invalid response body: {error}")))?;
        let sequence = parsed
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| PutError::Retryable(format!("invalid response body: {body}")))?;
        return Ok(sequence);
    }
    let message = format!("status {status} {}", body.trim());
    // 認証と入力の誤りは、再試行しても直らない
    if matches!(status.as_u16(), 400 | 401 | 409) {
        Err(PutError::Fatal(message))
    } else {
        Err(PutError::Retryable(message))
    }
}

/// セグメントごとに重ならない識別子を作る
fn new_identifier() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("failed to generate an identifier: {error}"))?;
    Ok(hex_encode(&bytes))
}

/// セットに含まれるファイルを消す
fn delete_files(set: &PendingSet) {
    for file in &set.files {
        let _ = fs::remove_file(&file.path);
    }
}

/// 停止要求を確認しながら待つ
fn sleep_interruptible(duration: Duration, stop: &AtomicBool, channel_stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) || channel_stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EncoderSettings, InputSettings, Resolution};

    /// テストで使う rendition
    fn rendition(quality: &str) -> RenditionSettings {
        RenditionSettings {
            quality: quality.to_owned(),
            mode: RenditionMode::Copy,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
            bandwidth: 12_000_000,
            video_bitrate: None,
        }
    }

    /// テストで使う設定
    fn settings() -> PublisherSettings {
        PublisherSettings {
            segment_duration_ms: 6000,
            backlog_limit: 6,
            encoder: EncoderSettings {
                codec: "h264_nvenc".to_owned(),
                preset: "p5".to_owned(),
            },
            controller_url: "http://127.0.0.1:9000".to_owned(),
            auth_token: Some("example-token".to_owned()),
            key_file: None,
            stream_credentials_url: None,
            stream_ingest_token: None,
            inputs: vec![InputSettings {
                channel_id: "1A".to_owned(),
                input_url: "srt://0.0.0.0:9101?mode=listener".to_owned(),
                live_url: None,
                key_uri: None,
                renditions: vec![rendition("1080p")],
            }],
        }
    }

    #[test]
    fn encrypts_with_aes_128_cbc_and_pkcs7() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let encrypted = encrypt_segment(&key, &[0u8; 16], b"edamame publisher");
        assert_eq!(
            hex_encode(&encrypted),
            "da74cde630d6ff687b23dbd0a91fa70751502083a6ee5b15f17f1e3cfdbf6393"
        );
    }

    #[test]
    fn pads_to_the_block_size() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        assert_eq!(encrypt_segment(&key, &iv, &[0u8; 16]).len(), 32);
        assert_eq!(encrypt_segment(&key, &iv, &[0u8; 15]).len(), 16);
        assert_eq!(encrypt_segment(&key, &iv, &[]).len(), 16);
    }

    #[test]
    fn builds_segment_url() {
        let url = segment_url("http://127.0.0.1:9000", "1A", "1080p", "abc123");
        assert_eq!(url, "http://127.0.0.1:9000/segments/1A/1080p/abc123");
    }

    #[test]
    fn parses_master_qualities() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=6500000,RESOLUTION=1280x720\n720p.m3u8\n";
        assert_eq!(parse_master_qualities(text), vec!["720p".to_owned()]);
    }

    #[test]
    fn parses_playlist_sequences() {
        let text = "#EXTM3U\n#EXTINF:6.000,\nhttp://127.0.0.1:9001/segments/1A/720p/7.ts\n\
                    #EXTINF:6.000,\nhttp://127.0.0.1:9001/segments/1A/720p/8.ts\n";
        assert_eq!(parse_playlist_sequences(text), vec![7, 8]);
    }

    #[test]
    fn parses_segment_index() {
        assert_eq!(parse_segment_index("12.ts"), Some(12));
        assert_eq!(parse_segment_index("12.ts.tmp"), None);
        assert_eq!(parse_segment_index("index.m3u8"), None);
    }

    #[test]
    fn adds_key_uri_and_iv_to_segment_query() {
        let mut settings = settings();
        settings.inputs[0].key_uri = Some("https://live.test/key/1A".to_owned());
        let context = ChannelContext {
            input_index: 0,
            settings: Arc::new(settings),
            key: [0u8; 16],
            work_dir: PathBuf::from("/tmp"),
        };
        let iv = "0000000000000000000000000000000a";
        let query = segment_query(&context, &rendition("1080p"), iv);
        assert!(query.contains(&("duration_ms", "6000".to_owned())));
        assert!(query.contains(&("bandwidth", "12000000".to_owned())));
        assert!(query.contains(&("resolution", "1920x1080".to_owned())));
        assert!(query.contains(&("key_uri", "https://live.test/key/1A".to_owned())));
        assert!(query.contains(&("iv", iv.to_owned())));
    }

    #[test]
    fn omits_key_uri_and_iv_without_key_uri() {
        let context = ChannelContext {
            input_index: 0,
            settings: Arc::new(settings()),
            key: [0u8; 16],
            work_dir: PathBuf::from("/tmp"),
        };
        let query = segment_query(
            &context,
            &rendition("1080p"),
            "0000000000000000000000000000000a",
        );
        assert!(!query.iter().any(|(name, _)| *name == "key_uri"));
        assert!(!query.iter().any(|(name, _)| *name == "iv"));
    }

    #[test]
    fn continues_run_numbers_after_the_runs_left_on_disk() {
        // 前の実行が残した run を新しい実行が読み直さないよう、番号を続きから始める
        let dir =
            std::env::temp_dir().join(format!("edamame-publisher-run-{}", std::process::id()));
        fs::create_dir_all(dir.join("run-2")).unwrap();
        fs::create_dir_all(dir.join("run-7")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();

        assert_eq!(last_run_number(&dir), 7);
        // 作業ディレクトリがない channel は 0 から始める
        assert_eq!(last_run_number(&dir.join("missing")), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_and_reuses_key_file() {
        let dir =
            std::env::temp_dir().join(format!("edamame-publisher-key-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.bin");
        let _ = fs::remove_file(&path);

        let created = load_or_create_key(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), created);
        // 2 回目は同じ鍵を読む
        assert_eq!(load_or_create_key(&path).unwrap(), created);

        // 16 byte でないファイルはエラーにする
        fs::write(&path, b"short").unwrap();
        assert!(load_or_create_key(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn identifies_complete_segments() {
        let context = ChannelContext {
            input_index: 0,
            settings: Arc::new(PublisherSettings {
                inputs: vec![InputSettings {
                    channel_id: "1A".to_owned(),
                    input_url: "srt://0.0.0.0:9101?mode=listener".to_owned(),
                    live_url: None,
                    key_uri: None,
                    renditions: vec![rendition("1080p"), rendition("720p")],
                }],
                ..settings()
            }),
            key: [0u8; 16],
            work_dir: PathBuf::from("/tmp"),
        };
        let mut run = RunState::new(PathBuf::from("/tmp/run-1"));
        run.files
            .entry(0)
            .or_default()
            .insert("1080p".to_owned(), PathBuf::from("0.ts"));
        run.files
            .entry(1)
            .or_default()
            .insert("1080p".to_owned(), PathBuf::from("1.ts"));
        run.files
            .entry(1)
            .or_default()
            .insert("720p".to_owned(), PathBuf::from("1.ts"));
        assert_eq!(next_complete_index(&context, &run), Some(1));
    }
}
