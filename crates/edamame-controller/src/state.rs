//! controller のメモリ状態、インジェスト、registry、playlist 生成

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;

use edamame_domain::{
    EdamameError, QualityPlaylistInput, SegmentEncryption, SegmentId, SegmentKey, SyncResponse,
    normalize_external_m3u8, normalize_iv, normalize_key_uri, normalize_public_url, now_ms,
    plan_master_playlist, plan_media_playlist, render_master_playlist, render_media_playlist,
    validate_identifier, validate_quality, validate_resolution,
};
use tokio::sync::Mutex;

use crate::external;
use crate::storage::CanonicalStore;

/// controller の共有状態
pub struct ControllerState {
    pub store: CanonicalStore,
    pub playlist_segments: u64,
    pub sync_interval_ms: u64,
    pub sync_timeout_ms: u64,
    pub publish_wait_ms: u64,
    /// 外部配信へ流す最低の割合（%）
    pub external_min_percent: u8,
    pub inner: Mutex<Inner>,
    /// セグメントの属性確認と番号の割り当てからメタデータの更新までを直列化する
    write_lock: Mutex<()>,
}

/// メモリ上の channel と worker
#[derive(Default)]
pub struct Inner {
    pub channels: BTreeMap<String, ChannelState>,
    pub workers: BTreeMap<String, WorkerState>,
}

/// channel のメタデータ
pub struct ChannelState {
    pub segment_duration_ms: u64,
    pub qualities: BTreeMap<String, QualityState>,
    /// 外部配信の master m3u8 の URL。publisher が PUT で宣言する
    pub external_m3u8: Option<String>,
    /// セグメント識別子から割り当てたセグメント番号への対応
    pub identifiers: BTreeMap<String, IdentifierState>,
    /// 次に新しい識別子へ割り当てるセグメント番号
    pub next_sequence: u64,
}

/// セグメント識別子に割り当てた番号と、セグメントファイルを保存した quality
pub struct IdentifierState {
    pub sequence: u64,
    pub qualities: BTreeSet<String>,
}

impl ChannelState {
    /// channel の状態を作る
    fn new(segment_duration_ms: u64, next_sequence: u64) -> Self {
        Self {
            segment_duration_ms,
            qualities: BTreeMap::new(),
            external_m3u8: None,
            identifiers: BTreeMap::new(),
            next_sequence,
        }
    }
}

/// quality のメタデータ
#[derive(Default)]
pub struct QualityState {
    pub bandwidth: u64,
    pub resolution: Option<String>,
    /// セグメント番号ごとの暗号化の指定。登録がないセグメントは暗号化されていない
    pub encryption: BTreeMap<u64, SegmentEncryption>,
    /// 配信先の選択で使う worker ごとの現在値
    pub current_weights: BTreeMap<String, i64>,
    /// 公開済みのセグメント番号
    pub published: BTreeSet<u64>,
    /// 未公開セグメントの公開待ちが終わる時刻
    pub publish_due_ms: Option<u64>,
    /// 公開待ちが終わった最大のセグメント番号
    pub allowed_through: Option<u64>,
    /// 公開待ちが終わっていないセグメント番号
    pub pending: BTreeSet<u64>,
    /// メタデータ上に残っている直近のセグメント番号
    pub segments: BTreeSet<u64>,
}

/// worker の登録内容と最後の報告
pub struct WorkerState {
    pub public_url: String,
    /// 相対的な配信能力
    pub weight: i64,
    pub last_sync_ms: u64,
    pub cached: BTreeSet<SegmentId>,
    /// worker が報告した取得中のセグメントであり、暗号化の指定を残すために使う
    pub fetching: BTreeSet<SegmentId>,
    /// 直前に指示した取得対象であり、報告が届くまでの間だけ暗号化の指定を残すために使う
    pub instructed: BTreeSet<SegmentId>,
}

/// セグメント登録の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// 新しいセグメントファイルを保存した
    Created { sequence: u64 },
    /// 同じ識別子のセグメントファイルがすでにあり、バイト列も同じである
    AlreadyExists { sequence: u64 },
}

/// セグメント登録の入力
pub struct SegmentIngest<'a> {
    pub channel_id: &'a str,
    pub quality: &'a str,
    pub identifier: &'a str,
    pub duration_ms: u64,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    pub key_uri: Option<&'a str>,
    pub iv: Option<&'a str>,
    pub bytes: &'a [u8],
}

impl ControllerState {
    /// 共有状態を作る
    #[must_use]
    pub fn new(
        store: CanonicalStore,
        playlist_segments: u64,
        sync_interval_ms: u64,
        sync_timeout_ms: u64,
        publish_wait_ms: u64,
        external_min_percent: u8,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms,
            external_min_percent,
            inner: Mutex::new(Inner::default()),
            write_lock: Mutex::new(()),
        })
    }

    /// channel の外部配信の master m3u8 を登録する。`None` で取り消す
    pub async fn set_external_m3u8(
        &self,
        channel_id: &str,
        external_m3u8: Option<&str>,
    ) -> Result<(), EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        // 形式だけを確認する。取得するかどうかは client に任せる
        let external_m3u8 = external_m3u8.map(normalize_external_m3u8).transpose()?;
        let mut inner = self.inner.lock().await;
        let channel = inner
            .channels
            .get_mut(channel_id)
            .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
        channel.external_m3u8 = external_m3u8;
        Ok(())
    }

    /// セグメントを検証し、識別子にセグメント番号を割り当てて保存する
    pub async fn ingest_segment(
        &self,
        input: SegmentIngest<'_>,
    ) -> Result<IngestOutcome, EdamameError> {
        let SegmentIngest {
            channel_id,
            quality,
            identifier,
            duration_ms,
            bandwidth,
            resolution,
            key_uri,
            iv,
            bytes,
        } = input;
        validate_identifier("channel_id", channel_id)?;
        validate_quality(quality)?;
        validate_identifier("identifier", identifier)?;
        if duration_ms < 500 {
            return Err(EdamameError::BadRequest(
                "duration_ms must be at least 500".to_owned(),
            ));
        }
        if bandwidth == 0 {
            return Err(EdamameError::BadRequest(
                "bandwidth must be positive".to_owned(),
            ));
        }
        if let Some(value) = &resolution {
            validate_resolution(value)?;
        }
        // 鍵は publisher が配信するため、URL としての形式だけを確認する
        let key_uri = key_uri.map(normalize_key_uri).transpose()?;
        // IV は鍵と組で使うため、鍵 URI なしでは指定できない
        let iv = match iv {
            Some(value) => {
                if key_uri.is_none() {
                    return Err(EdamameError::BadRequest("iv requires key_uri".to_owned()));
                }
                Some(normalize_iv(value)?)
            }
            None => None,
        };

        // 属性の確認と番号の割り当てを直列化する。ファイル I/O の間も inner の mutex は保持しない。
        let _write_guard = self.write_lock.lock().await;
        // 保存に成功したときだけメタデータを確定するため、ここでは割り当てる番号を決めるだけにする
        let (sequence, known_identifier, stored_in_quality, next_sequence) = {
            let inner = self.inner.lock().await;
            validate_segment_metadata(
                &inner,
                channel_id,
                quality,
                duration_ms,
                bandwidth,
                resolution.as_deref(),
            )?;
            match inner.channels.get(channel_id) {
                Some(channel) => match channel.identifiers.get(identifier) {
                    Some(state) => (
                        state.sequence,
                        true,
                        // 同じ quality に保存済みの場合だけ、バイト列を比較する
                        state.qualities.contains(quality),
                        channel.next_sequence,
                    ),
                    None => (
                        channel.next_sequence,
                        false,
                        false,
                        channel.next_sequence.checked_add(1).ok_or_else(|| {
                            EdamameError::Internal("segment sequence is exhausted".to_owned())
                        })?,
                    ),
                },
                // controller の起動ごとに 0 から採番する
                None => (0, false, false, 1),
            }
        };

        let outcome = if stored_in_quality {
            // 同じ識別子の再 PUT は、保存済みのバイト列と同じ場合だけ受け付ける
            if self
                .store
                .segment_exists(channel_id, quality, sequence)
                .await?
            {
                let existing = self
                    .store
                    .read_segment(channel_id, quality, sequence)
                    .await?;
                if existing != bytes {
                    return Err(EdamameError::Conflict(
                        "segment bytes differ from the canonical file".to_owned(),
                    ));
                }
                IngestOutcome::AlreadyExists { sequence }
            } else {
                self.store
                    .write_segment(channel_id, quality, sequence, bytes)
                    .await?;
                IngestOutcome::Created { sequence }
            }
        } else {
            // この quality に初めて保存する。前の実行のファイルと同じ番号になっても上書きする
            self.store
                .write_segment(channel_id, quality, sequence, bytes)
                .await?;
            IngestOutcome::Created { sequence }
        };

        // 保存が終わる前に待ち時間を始めると、worker が取得する時間を失う
        let committed_at_ms = now_ms();
        let mut inner = self.inner.lock().await;
        // worker が持つ、または取得を指示したセグメントの暗号化の指定も、公開範囲が古い範囲へ戻る場合のために残す
        let worker_segments: BTreeSet<u64> = inner
            .workers
            .values()
            .flat_map(|worker| {
                worker
                    .cached
                    .iter()
                    .chain(worker.fetching.iter())
                    .chain(worker.instructed.iter())
            })
            .filter(|id| id.channel_id == channel_id && id.quality == quality)
            .map(|id| id.sequence)
            .collect();
        let channel = inner
            .channels
            .entry(channel_id.to_owned())
            .or_insert_with(|| ChannelState::new(duration_ms, next_sequence));
        let identifier_state = channel
            .identifiers
            .entry(identifier.to_owned())
            .or_insert_with(|| IdentifierState {
                sequence,
                qualities: BTreeSet::new(),
            });
        identifier_state.qualities.insert(quality.to_owned());
        if !known_identifier {
            channel.next_sequence = next_sequence;
        }
        let quality_state = channel
            .qualities
            .entry(quality.to_owned())
            .or_insert_with(|| QualityState {
                bandwidth,
                resolution: resolution.clone(),
                encryption: BTreeMap::new(),
                current_weights: BTreeMap::new(),
                published: BTreeSet::new(),
                publish_due_ms: None,
                allowed_through: None,
                pending: BTreeSet::new(),
                segments: BTreeSet::new(),
            });
        if quality_state.resolution.is_none() {
            quality_state.resolution = resolution;
        }
        // 暗号化の指定は省略できるため、省略されたセグメントは表から消して暗号化なしに戻す
        match key_uri {
            Some(key_uri) => {
                quality_state
                    .encryption
                    .insert(sequence, SegmentEncryption { key_uri, iv });
            }
            None => {
                quality_state.encryption.remove(&sequence);
            }
        }
        register_segment_metadata(
            quality_state,
            sequence,
            committed_at_ms,
            self.publish_wait_ms,
            known_identifier && stored_in_quality,
        );
        prune_segments(&mut quality_state.segments, self.playlist_segments);
        prune_encryption(
            &mut quality_state.encryption,
            &quality_state.segments,
            &worker_segments,
        );
        Ok(outcome)
    }

    /// worker を登録または更新する。新規なら `true`。
    pub async fn register_worker(
        &self,
        worker_id: &str,
        public_url: &str,
        weight: u32,
        now_ms: u64,
    ) -> Result<bool, EdamameError> {
        validate_identifier("worker_id", worker_id)?;
        if weight == 0 {
            return Err(EdamameError::BadRequest(
                "weight must be positive".to_owned(),
            ));
        }
        let public_url = normalize_public_url(public_url)?;
        let mut inner = self.inner.lock().await;
        let created = if let Some(worker) = inner.workers.get_mut(worker_id) {
            worker.public_url = public_url;
            worker.weight = i64::from(weight);
            // worker は登録のたびにキャッシュを捨てるため、報告済みのキャッシュも無効にする
            worker.cached = BTreeSet::new();
            worker.fetching = BTreeSet::new();
            worker.instructed = BTreeSet::new();
            false
        } else {
            inner.workers.insert(
                worker_id.to_owned(),
                WorkerState {
                    public_url,
                    weight: i64::from(weight),
                    last_sync_ms: now_ms,
                    cached: BTreeSet::new(),
                    fetching: BTreeSet::new(),
                    instructed: BTreeSet::new(),
                },
            );
            true
        };
        // 重みが変わると前の周期が合わなくなるため、全 quality の現在値をやり直す
        reset_current_weights(&mut inner.channels);
        Ok(created)
    }

    /// sync を受け付けて取得一覧を返す
    pub async fn sync_worker(
        &self,
        worker_id: &str,
        cached_segments: Vec<SegmentKey>,
        fetching_segments: Vec<SegmentKey>,
        now_ms: u64,
    ) -> Result<SyncResponse, EdamameError> {
        validate_identifier("worker_id", worker_id)?;
        let mut cached = BTreeSet::new();
        for key in cached_segments {
            cached.insert(key.into_id()?);
        }
        let mut fetching = BTreeSet::new();
        for key in fetching_segments {
            fetching.insert(key.into_id()?);
        }
        let mut inner = self.inner.lock().await;
        {
            let worker = inner
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| EdamameError::NotFound("worker is not registered".to_owned()))?;
            worker.last_sync_ms = now_ms;
            worker.cached = cached;
            worker.fetching = fetching;
        }
        // 公開範囲は GET では変えず、worker の sync 報告を受けた時だけ更新する
        {
            let Inner { channels, workers } = &mut *inner;
            advance_published(
                channels,
                workers,
                self.playlist_segments,
                self.sync_timeout_ms,
                now_ms,
            );
        }
        let target = cache_target(&inner.channels, self.playlist_segments);
        let worker = inner
            .workers
            .get_mut(worker_id)
            // 上の get_mut で存在を確認済みのため、ここでは見つからないことがない
            .expect("worker was checked above");
        let missing: BTreeSet<SegmentId> = target.difference(&worker.cached).cloned().collect();
        let fetch_segments = missing.iter().map(SegmentKey::from).collect();
        // 指示した一覧は、worker が取得中として報告するか、キャッシュとして報告するまで残す
        worker.instructed = missing;
        Ok(SyncResponse { fetch_segments })
    }

    /// メディアプレイリストを生成する
    pub async fn media_playlist(
        &self,
        channel_id: &str,
        quality: &str,
        now_ms: u64,
    ) -> Result<String, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        validate_quality(quality)?;
        let (segment_duration_ms, published, encryption, worker_url) = {
            let mut inner = self.inner.lock().await;
            let Inner { channels, workers } = &mut *inner;
            let channel = channels
                .get_mut(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let quality_state = channel
                .qualities
                .get_mut(quality)
                .ok_or_else(|| EdamameError::NotFound("quality not found".to_owned()))?;
            if quality_state.published.is_empty() {
                return Err(EdamameError::Unavailable(
                    "playlist is not published yet".to_owned(),
                ));
            }
            let worker_url = select_serving_worker(
                workers,
                &mut quality_state.current_weights,
                channel_id,
                quality,
                &quality_state.published,
                now_ms,
                self.sync_timeout_ms,
            );
            (
                channel.segment_duration_ms,
                quality_state.published.clone(),
                // 表は公開に使う範囲と worker のキャッシュの範囲に間引いてあるため、そのまま複製する
                quality_state.encryption.clone(),
                worker_url,
            )
        };
        let playlist =
            plan_media_playlist(&published, segment_duration_ms, self.playlist_segments)?;
        for sequence in &playlist.sequences {
            if !self
                .store
                .segment_exists(channel_id, quality, *sequence)
                .await?
            {
                return Err(EdamameError::Unprocessable(
                    "segment file is missing".to_owned(),
                ));
            }
        }
        // 422 を 503 より先に返すため、ファイル確認後に worker 不在を判定する
        let public_url = worker_url.ok_or_else(|| {
            EdamameError::Unavailable("no worker has the published range".to_owned())
        })?;
        Ok(render_media_playlist(
            &playlist,
            channel_id,
            quality,
            &public_url,
            &encryption,
        ))
    }

    /// マスタープレイリストを生成する。外部配信へ流す場合は転送先の URL を返す
    ///
    /// 外部配信へ振り分けるかどうかは `routing` で指定する。外部配信が登録されていなければ、
    /// 振り分ける指定でも自前のマスタープレイリストを返す。
    pub async fn master_playlist(
        &self,
        channel_id: &str,
        now_ms: u64,
        routing: ExternalRouting,
    ) -> Result<MasterResponse, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        // ファイル確認と外部への確認の間は mutex を保持せず、公開範囲と配信可否を先に集める
        let (segment_duration_ms, published, external_m3u8, external_percent) = {
            let inner = self.inner.lock().await;
            let Inner { channels, workers } = &*inner;
            let channel = channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let mut published = BTreeMap::new();
            let mut external_percent = 0;
            for (quality, quality_state) in &channel.qualities {
                if quality_state.published.is_empty() {
                    continue;
                }
                let served = workers.values().any(|worker| {
                    is_alive(worker, now_ms, self.sync_timeout_ms)
                        && holds_segments(worker, channel_id, quality, &quality_state.published)
                });
                if served {
                    // 公開範囲が最新セグメントに追いついていないほど、外部へ流す割合を上げる
                    let latest_published = quality_state
                        .published
                        .iter()
                        .next_back()
                        .copied()
                        .unwrap_or_default();
                    let latest_segment = quality_state
                        .segments
                        .iter()
                        .next_back()
                        .copied()
                        .unwrap_or(latest_published);
                    let published_delay = latest_segment.saturating_sub(latest_published);
                    let normal_delay = self
                        .publish_wait_ms
                        .div_ceil(channel.segment_duration_ms.max(1));
                    external_percent = external_percent.max(external::external_routing_percent(
                        published_delay,
                        normal_delay,
                        self.playlist_segments,
                        self.external_min_percent,
                    ));
                }
                published.insert(
                    quality.clone(),
                    (
                        quality_state.bandwidth,
                        quality_state.resolution.clone(),
                        quality_state.published.clone(),
                        served,
                    ),
                );
            }
            // 配信できる quality が 1 つもないときは全量を外部へ逃がす
            if !published.values().any(|(_, _, _, served)| *served) {
                external_percent = 100;
            }
            (
                channel.segment_duration_ms,
                published,
                channel.external_m3u8.clone(),
                external_percent,
            )
        };
        let route_external = match routing {
            ExternalRouting::LocalOnly => false,
            ExternalRouting::ExternalOnly => true,
            ExternalRouting::ByClient(client_ip) => {
                external::routes_to_external(channel_id, client_ip, external_percent)
            }
        };
        if route_external && let Some(url) = &external_m3u8 {
            return Ok(MasterResponse::External(url.clone()));
        }
        if published.is_empty() {
            return Err(EdamameError::Unavailable(
                "playlist is not published yet".to_owned(),
            ));
        }
        for (quality, (_, _, segments, _)) in &published {
            for sequence in segments {
                if !self
                    .store
                    .segment_exists(channel_id, quality, *sequence)
                    .await?
                {
                    return Err(EdamameError::Unprocessable(
                        "segment file is missing".to_owned(),
                    ));
                }
            }
        }
        // 422 を 503 より先に返すため、ファイル確認後に worker 不在を判定する
        let mut inputs = BTreeMap::new();
        for (quality, (bandwidth, resolution, segments, served)) in &published {
            if !*served {
                continue;
            }
            inputs.insert(
                quality.clone(),
                QualityPlaylistInput {
                    bandwidth: *bandwidth,
                    resolution: resolution.clone(),
                    segments: segments.clone(),
                },
            );
        }
        if inputs.is_empty() {
            return Err(EdamameError::Unavailable(
                "no quality can be served".to_owned(),
            ));
        }
        let entries = plan_master_playlist(&inputs, segment_duration_ms, self.playlist_segments)?;
        Ok(MasterResponse::Local(render_master_playlist(&entries)))
    }
}

/// `master.m3u8` を外部配信へ振り分けるかどうかの指定
pub enum ExternalRouting {
    /// 接続元ごとの割合で判定する
    ByClient(IpAddr),
    /// 振り分けず、自前のマスタープレイリストを返す
    LocalOnly,
    /// 外部配信へ振り分ける
    ExternalOnly,
}

/// `master.m3u8` の応答
pub enum MasterResponse {
    /// 自前のマスタープレイリストを返す
    Local(String),
    /// 外部配信の master m3u8 へ転送する
    External(String),
}

/// 既存の channel と quality の属性に入力が一致するか確認する
fn validate_segment_metadata(
    inner: &Inner,
    channel_id: &str,
    quality: &str,
    duration_ms: u64,
    bandwidth: u64,
    resolution: Option<&str>,
) -> Result<(), EdamameError> {
    let Some(channel) = inner.channels.get(channel_id) else {
        return Ok(());
    };
    if channel.segment_duration_ms != duration_ms {
        return Err(EdamameError::Conflict(
            "segment duration differs from the channel".to_owned(),
        ));
    }
    let Some(existing) = channel.qualities.get(quality) else {
        return Ok(());
    };
    if existing.bandwidth != bandwidth {
        return Err(EdamameError::Conflict(
            "bandwidth differs from the quality".to_owned(),
        ));
    }
    if let (Some(established), Some(incoming)) = (&existing.resolution, resolution)
        && established != incoming
    {
        return Err(EdamameError::Conflict(
            "resolution differs from the quality".to_owned(),
        ));
    }
    Ok(())
}

/// 期限内に sync した worker か
fn is_alive(worker: &WorkerState, now_ms: u64, sync_timeout_ms: u64) -> bool {
    now_ms.saturating_sub(worker.last_sync_ms) <= sync_timeout_ms
}

/// 全 quality の選択の周期をやり直す
fn reset_current_weights(channels: &mut BTreeMap<String, ChannelState>) {
    for channel in channels.values_mut() {
        for quality_state in channel.qualities.values_mut() {
            quality_state.current_weights.clear();
        }
    }
}

/// 公開範囲を保持している worker から重みの比に従って 1 台選び、公開 URL を返す
fn select_serving_worker(
    workers: &BTreeMap<String, WorkerState>,
    current_weights: &mut BTreeMap<String, i64>,
    channel_id: &str,
    quality: &str,
    published: &BTreeSet<u64>,
    now_ms: u64,
    sync_timeout_ms: u64,
) -> Option<String> {
    let candidates: Vec<&str> = workers
        .iter()
        .filter(|(_, worker)| {
            is_alive(worker, now_ms, sync_timeout_ms)
                && holds_segments(worker, channel_id, quality, published)
        })
        .map(|(worker_id, _)| worker_id.as_str())
        .collect();
    if candidates.is_empty() {
        current_weights.clear();
        return None;
    }

    // 候補が増減したら、その時点から重みの比で選び直す
    let keep_cycle = candidates.len() == current_weights.len()
        && candidates
            .iter()
            .all(|worker_id| current_weights.contains_key(*worker_id));
    if !keep_cycle {
        current_weights.clear();
    }
    let total_weight: i64 = candidates
        .iter()
        .map(|worker_id| workers[*worker_id].weight)
        .sum();

    // 現在値に重みを足し、最大の worker に配る。選んだ worker からは合計重みを引く
    let mut selected: Option<&str> = None;
    let mut highest = i64::MIN;
    for worker_id in &candidates {
        let weight = workers[*worker_id].weight;
        let current = current_weights.entry((*worker_id).to_owned()).or_insert(0);
        *current += weight;
        // 現在値が同じときは worker_id の昇順で先頭を残す
        if *current > highest {
            highest = *current;
            selected = Some(*worker_id);
        }
    }

    let selected = selected?;
    if let Some(current) = current_weights.get_mut(selected) {
        *current -= total_weight;
    }
    workers
        .get(selected)
        .map(|worker| worker.public_url.clone())
}

/// worker が指定された quality のセグメントをすべて保持しているか
fn holds_segments(
    worker: &WorkerState,
    channel_id: &str,
    quality: &str,
    segments: &BTreeSet<u64>,
) -> bool {
    segments.iter().all(|sequence| {
        worker.cached.contains(&SegmentId {
            channel_id: channel_id.to_owned(),
            quality: quality.to_owned(),
            sequence: *sequence,
        })
    })
}

/// 各 quality の公開範囲を独立に進める
fn advance_published(
    channels: &mut BTreeMap<String, ChannelState>,
    workers: &BTreeMap<String, WorkerState>,
    playlist_segments: u64,
    sync_timeout_ms: u64,
    now_ms: u64,
) {
    for (channel_id, channel) in channels.iter_mut() {
        for (quality, quality_state) in channel.qualities.iter_mut() {
            advance_quality_published(
                channel_id,
                quality,
                quality_state,
                workers,
                playlist_segments,
                sync_timeout_ms,
                now_ms,
            );
        }
    }
}

/// 1 つの quality の公開範囲を、保持している worker の最新範囲に合わせる
fn advance_quality_published(
    channel_id: &str,
    quality: &str,
    quality_state: &mut QualityState,
    workers: &BTreeMap<String, WorkerState>,
    playlist_segments: u64,
    sync_timeout_ms: u64,
    now_ms: u64,
) {
    // 公開待ちが終わったら、その時点のメタデータ最新まで公開可能にする
    advance_allowed(quality_state, now_ms);
    if quality_state.allowed_through.is_none() {
        return;
    }
    let candidates = held_candidates(
        channel_id,
        quality,
        quality_state,
        workers,
        playlist_segments,
        sync_timeout_ms,
        now_ms,
    );
    let Some(candidate) = candidates.into_iter().next() else {
        return;
    };
    let candidate_latest = candidate
        .iter()
        .next_back()
        .copied()
        .expect("candidate is not empty");
    let current_served = !quality_state.published.is_empty()
        && workers.values().any(|worker| {
            is_alive(worker, now_ms, sync_timeout_ms)
                && holds_segments(worker, channel_id, quality, &quality_state.published)
        });
    if current_served
        && quality_state
            .published
            .iter()
            .next_back()
            .is_some_and(|published_latest| candidate_latest <= *published_latest)
    {
        return;
    }
    quality_state.published = candidate;
}

/// 公開待ちが終わっていれば、その時点のメタデータにあるセグメントを公開可能にする
fn advance_allowed(quality_state: &mut QualityState, now_ms: u64) {
    let Some(due_ms) = quality_state.publish_due_ms else {
        return;
    };
    if now_ms < due_ms {
        return;
    }
    if let Some(latest) = quality_state.segments.iter().next_back().copied() {
        quality_state.allowed_through = Some(
            quality_state
                .allowed_through
                .map_or(latest, |through| through.max(latest)),
        );
    }
    if let Some(through) = quality_state.allowed_through {
        quality_state.pending.retain(|sequence| *sequence > through);
    }
    quality_state.publish_due_ms = None;
}

/// セグメントを quality のメタデータに加え、必要なら公開待ちを始める
fn register_segment_metadata(
    quality_state: &mut QualityState,
    sequence: u64,
    now_ms: u64,
    publish_wait_ms: u64,
    already_known_for_quality: bool,
) {
    // 期限切れの公開待ちを確定してから、新しいセグメントの待ち時間を始める
    advance_allowed(quality_state, now_ms);
    quality_state.segments.insert(sequence);
    if already_known_for_quality {
        return;
    }
    quality_state.pending.insert(sequence);
    if quality_state.publish_due_ms.is_none() {
        quality_state.publish_due_ms = Some(now_ms.saturating_add(publish_wait_ms));
    }
}

/// セグメントが公開可能か
fn is_allowed(allowed_through: Option<u64>, pending: &BTreeSet<u64>, sequence: u64) -> bool {
    allowed_through.is_some_and(|through| sequence <= through) && !pending.contains(&sequence)
}

/// 期限内に sync した worker が連続して保持している最新範囲を返す
fn held_candidates(
    channel_id: &str,
    quality: &str,
    quality_state: &QualityState,
    workers: &BTreeMap<String, WorkerState>,
    playlist_segments: u64,
    sync_timeout_ms: u64,
    now_ms: u64,
) -> Vec<BTreeSet<u64>> {
    let mut ranges = Vec::new();
    for worker in workers
        .values()
        .filter(|worker| is_alive(worker, now_ms, sync_timeout_ms))
    {
        for (first, last) in worker_held_ranges(
            channel_id,
            quality,
            &worker.cached,
            quality_state.allowed_through,
            &quality_state.pending,
        ) {
            let start = first.max(last.saturating_sub(playlist_segments.saturating_sub(1)));
            ranges.push((start, last));
        }
    }
    ranges.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    ranges.dedup();
    ranges
        .into_iter()
        .map(|(first, last)| (first..=last).collect())
        .collect()
}

/// worker が保持している連続範囲を、新しい順に返す
fn worker_held_ranges(
    channel_id: &str,
    quality: &str,
    cached: &BTreeSet<SegmentId>,
    allowed_through: Option<u64>,
    pending: &BTreeSet<u64>,
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut first = None;
    let mut last = None;
    for id in cached.iter().rev() {
        if id.channel_id.as_str() != channel_id
            || id.quality.as_str() != quality
            || !is_allowed(allowed_through, pending, id.sequence)
        {
            continue;
        }
        match (first, last) {
            (Some(range_first), Some(current_last)) => {
                if id.sequence.checked_add(1) == Some(range_first) {
                    first = Some(id.sequence);
                } else {
                    ranges.push((range_first, current_last));
                    first = Some(id.sequence);
                    last = Some(id.sequence);
                }
            }
            _ => {
                first = Some(id.sequence);
                last = Some(id.sequence);
            }
        }
    }
    if let (Some(first), Some(last)) = (first, last) {
        ranges.push((first, last));
    }
    ranges
}

/// メタデータ上に残す直近の範囲を計算する
fn prune_segments(segments: &mut BTreeSet<u64>, playlist_segments: u64) {
    let Some(latest) = segments.iter().next_back().copied() else {
        return;
    };
    let start = latest.saturating_sub(playlist_segments.saturating_sub(1));
    if start > 0 {
        *segments = segments.split_off(&start);
    }
}

/// 公開に使う可能性があるセグメントの暗号化の指定だけを残す
///
/// worker が持つセグメントと、取得を指示したセグメントを含めるのは、
/// 遅れた worker の古い公開範囲へ戻る場合に備えるためである。
fn prune_encryption(
    encryption: &mut BTreeMap<u64, SegmentEncryption>,
    segments: &BTreeSet<u64>,
    worker_segments: &BTreeSet<u64>,
) {
    encryption
        .retain(|sequence, _| segments.contains(sequence) || worker_segments.contains(sequence));
}

/// worker が取得すべき直近のセグメント範囲を返す
fn cache_target(
    channels: &BTreeMap<String, ChannelState>,
    playlist_segments: u64,
) -> BTreeSet<SegmentId> {
    let mut target = BTreeSet::new();
    for (channel_id, channel) in channels {
        for (quality, quality_state) in &channel.qualities {
            let Some(latest) = quality_state.segments.iter().next_back().copied() else {
                continue;
            };
            let start = latest.saturating_sub(playlist_segments.saturating_sub(1));
            for sequence in quality_state.segments.range(start..=latest) {
                target.insert(SegmentId {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    sequence: *sequence,
                });
            }
        }
    }
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 鍵 URI だけを指定する
    fn segment_encryption(key_uri: &str) -> SegmentEncryption {
        SegmentEncryption {
            key_uri: key_uri.to_owned(),
            iv: None,
        }
    }

    fn channel_with_quality(
        segments: &[u64],
        published: &[u64],
        publish_due_ms: Option<u64>,
        allowed_through: Option<u64>,
        pending: &[u64],
    ) -> ChannelState {
        let mut channel = ChannelState::new(6000, 0);
        add_quality(
            &mut channel,
            "720p",
            segments,
            published,
            publish_due_ms,
            allowed_through,
            pending,
        );
        channel
    }

    fn add_quality(
        channel: &mut ChannelState,
        quality: &str,
        segments: &[u64],
        published: &[u64],
        publish_due_ms: Option<u64>,
        allowed_through: Option<u64>,
        pending: &[u64],
    ) {
        channel.qualities.insert(
            quality.to_owned(),
            QualityState {
                bandwidth: 3_000_000,
                resolution: None,
                encryption: BTreeMap::new(),
                current_weights: BTreeMap::new(),
                published: published.iter().copied().collect(),
                publish_due_ms,
                allowed_through,
                pending: pending.iter().copied().collect(),
                segments: segments.iter().copied().collect(),
            },
        );
    }

    fn worker_with_cached(channel_id: &str, quality: &str, sequences: &[u64]) -> WorkerState {
        worker_with_cached_qualities(&[(channel_id, quality, sequences)])
    }

    fn worker_with_cached_qualities(entries: &[(&str, &str, &[u64])]) -> WorkerState {
        let mut cached = BTreeSet::new();
        for (channel_id, quality, sequences) in entries {
            for sequence in *sequences {
                cached.insert(SegmentId {
                    channel_id: (*channel_id).to_owned(),
                    quality: (*quality).to_owned(),
                    sequence: *sequence,
                });
            }
        }
        WorkerState {
            public_url: "http://worker.test".to_owned(),
            weight: 1,
            last_sync_ms: 0,
            cached,
            fetching: BTreeSet::new(),
            instructed: BTreeSet::new(),
        }
    }

    /// 重みを指定した worker を作る
    fn worker_with_weight(url: &str, weight: i64, sequences: &[u64]) -> WorkerState {
        let mut worker = worker_with_cached("live", "720p", sequences);
        worker.public_url = url.to_owned();
        worker.weight = weight;
        worker
    }

    /// セグメント番号ごとに違う IV を付けて登録する
    async fn ingest_encrypted_segment(state: &ControllerState, sequence: u64) {
        let iv = format!("{sequence:032x}");
        state
            .ingest_segment(SegmentIngest {
                channel_id: "live",
                quality: "720p",
                identifier: &format!("seg-{sequence}"),
                duration_ms: 6000,
                bandwidth: 3_000_000,
                resolution: None,
                key_uri: Some("https://keys.test/a"),
                iv: Some(&iv),
                bytes: b"segment",
            })
            .await
            .unwrap();
    }

    /// sync で送るセグメントの一覧を作る
    fn segment_keys(sequences: impl IntoIterator<Item = u64>) -> Vec<SegmentKey> {
        sequences
            .into_iter()
            .map(|sequence| SegmentKey {
                channel_id: "live".to_owned(),
                quality: "720p".to_owned(),
                sequence,
            })
            .collect()
    }

    /// worker が取得を指示されているセグメント番号
    async fn fetching_sequences(state: &ControllerState) -> Vec<u64> {
        let inner = state.inner.lock().await;
        inner.workers["worker-a"]
            .fetching
            .iter()
            .map(|id| id.sequence)
            .collect()
    }

    /// 取得に失敗したセグメントは、再取得の指示が残る間は暗号化の指定を残す
    #[tokio::test]
    async fn keeps_encryption_of_segments_to_refetch() {
        let dir = std::env::temp_dir().join(format!("edamame-refetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = ControllerState::new(CanonicalStore::new(dir.clone()), 6, 1_000, 10_000, 0, 1);
        for sequence in 0..6 {
            ingest_encrypted_segment(&state, sequence).await;
        }
        state
            .register_worker("worker-a", "http://worker.test", 1, 0)
            .await
            .unwrap();
        // 最初のセグメントだけ取得に失敗し、残りを取得した状態で報告する（0 は取得中）
        state
            .sync_worker("worker-a", segment_keys(1..6), segment_keys(0..1), 0)
            .await
            .unwrap();
        assert_eq!(fetching_sequences(&state).await, vec![0]);
        // 再取得の途中で新しいセグメントが届き、0 の取得が続いたまま再度 sync しても 0 は残る
        ingest_encrypted_segment(&state, 6).await;
        state
            .sync_worker("worker-a", segment_keys(1..6), segment_keys(0..1), 0)
            .await
            .unwrap();
        assert!(
            fetching_sequences(&state).await.contains(&0),
            "再取得中のセグメントが取得の対象から外れている"
        );
        // 再取得の指示が残ったまま、メタデータに残る範囲から外れるまでセグメントを追加する
        for sequence in 7..18 {
            ingest_encrypted_segment(&state, sequence).await;
        }
        // 再取得できた 0 から 5 を報告しても、0 の暗号化の指定が残っている
        state
            .sync_worker("worker-a", segment_keys(0..6), Vec::new(), 0)
            .await
            .unwrap();
        let sequences: Vec<u64> = {
            let inner = state.inner.lock().await;
            inner.channels["live"].qualities["720p"]
                .encryption
                .keys()
                .copied()
                .collect()
        };
        assert!(
            sequences.contains(&0),
            "再取得したセグメントの暗号化の指定が失われている: {sequences:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selects_serving_workers_by_weight() {
        // 重み 4 と 1 では、5 回のうち 1 回だけ軽い worker を選ぶ
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_weight("http://a.test", 4, &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_weight("http://b.test", 1, &[0, 1]),
            ),
        ]);
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut current_weights = BTreeMap::new();
        let mut selected = Vec::new();
        for _ in 0..5 {
            selected.push(
                select_serving_worker(
                    &workers,
                    &mut current_weights,
                    "live",
                    "720p",
                    &published,
                    0,
                    1_000,
                )
                .unwrap(),
            );
        }
        assert_eq!(
            selected,
            vec![
                "http://a.test",
                "http://a.test",
                "http://b.test",
                "http://a.test",
                "http://a.test"
            ]
        );
    }

    #[test]
    fn selects_same_weight_workers_alternately() {
        // 重みが同じなら順番に選ぶ
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_weight("http://a.test", 1, &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_weight("http://b.test", 1, &[0, 1]),
            ),
        ]);
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut current_weights = BTreeMap::new();
        let mut selected = Vec::new();
        for _ in 0..4 {
            selected.push(
                select_serving_worker(
                    &workers,
                    &mut current_weights,
                    "live",
                    "720p",
                    &published,
                    0,
                    1_000,
                )
                .unwrap(),
            );
        }
        assert_eq!(
            selected,
            vec![
                "http://a.test",
                "http://b.test",
                "http://a.test",
                "http://b.test"
            ]
        );
    }

    #[test]
    fn resets_current_weight_of_worker_without_published_range() {
        // 公開範囲を持たない worker は候補から外れ、現在値も消す
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_weight("http://a.test", 1, &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_weight("http://b.test", 1, &[1]),
            ),
        ]);
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut current_weights = BTreeMap::from([("worker-b".to_owned(), 5)]);
        assert_eq!(
            select_serving_worker(
                &workers,
                &mut current_weights,
                "live",
                "720p",
                &published,
                0,
                1_000
            )
            .unwrap(),
            "http://a.test"
        );
        assert!(!current_weights.contains_key("worker-b"));
    }

    #[test]
    fn resets_current_weight_of_timed_out_worker() {
        // sync が届いていない worker も候補から外れ、現在値を消す
        let mut workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_weight("http://a.test", 1, &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_weight("http://b.test", 1, &[0, 1]),
            ),
        ]);
        workers.get_mut("worker-a").unwrap().last_sync_ms = 5_000;
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut current_weights = BTreeMap::from([("worker-b".to_owned(), -5)]);
        assert_eq!(
            select_serving_worker(
                &workers,
                &mut current_weights,
                "live",
                "720p",
                &published,
                5_000,
                1_000
            )
            .unwrap(),
            "http://a.test"
        );
        assert!(!current_weights.contains_key("worker-b"));
    }

    #[test]
    fn resets_selection_when_candidate_set_changes() {
        // 3 台のうち 1 台が候補から外れたら、残った候補で重みの比をやり直す
        let mut workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_weight("http://a.test", 1, &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_weight("http://b.test", 1, &[0, 1]),
            ),
            (
                "worker-c".to_owned(),
                worker_with_weight("http://c.test", 1, &[0, 1]),
            ),
        ]);
        for worker in workers.values_mut() {
            worker.last_sync_ms = 5_000;
        }
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut current_weights = BTreeMap::new();
        assert_eq!(
            select_serving_worker(
                &workers,
                &mut current_weights,
                "live",
                "720p",
                &published,
                5_000,
                1_000
            )
            .unwrap(),
            "http://a.test"
        );
        // worker-c の sync が届かなくなる
        workers.get_mut("worker-c").unwrap().last_sync_ms = 0;
        let mut selected = Vec::new();
        for _ in 0..4 {
            let url = select_serving_worker(
                &workers,
                &mut current_weights,
                "live",
                "720p",
                &published,
                5_000,
                1_000,
            )
            .unwrap();
            selected.push(if url == "http://a.test" { "a" } else { "b" });
        }
        assert_eq!(selected, vec!["a", "b", "a", "b"]);
    }

    #[test]
    fn keeps_selection_state_per_quality() {
        // 720p の選択状態は、候補が違う 480p のリクエストで影響を受けない
        let mut workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_cached("live", "720p", &[0, 1]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_cached("live", "720p", &[0, 1]),
            ),
            (
                "worker-c".to_owned(),
                worker_with_cached("live", "480p", &[0, 1]),
            ),
        ]);
        workers.get_mut("worker-a").unwrap().public_url = "http://a.test".to_owned();
        workers.get_mut("worker-b").unwrap().public_url = "http://b.test".to_owned();
        workers.get_mut("worker-c").unwrap().public_url = "http://c.test".to_owned();
        let published: BTreeSet<u64> = [0, 1].into_iter().collect();
        let mut hd_weights = BTreeMap::new();
        let mut sd_weights = BTreeMap::new();
        let mut selected = Vec::new();
        for _ in 0..4 {
            // 480p の選択を挟んでも 720p は a、b の順に選ぶ
            assert_eq!(
                select_serving_worker(
                    &workers,
                    &mut sd_weights,
                    "live",
                    "480p",
                    &published,
                    0,
                    1_000
                )
                .unwrap(),
                "http://c.test"
            );
            let url = select_serving_worker(
                &workers,
                &mut hd_weights,
                "live",
                "720p",
                &published,
                0,
                1_000,
            )
            .unwrap();
            selected.push(if url == "http://a.test" { "a" } else { "b" });
        }
        assert_eq!(selected, vec!["a", "b", "a", "b"]);
    }

    #[tokio::test]
    async fn resets_current_weights_on_registration() {
        let state = ControllerState::new(
            CanonicalStore::new(std::path::PathBuf::from("/tmp/edamame-test")),
            6,
            1_000,
            10_000,
            0,
            1,
        );
        state
            .register_worker("worker-a", "http://a.test", 4, 0)
            .await
            .unwrap();
        state
            .register_worker("worker-b", "http://b.test", 1, 0)
            .await
            .unwrap();
        {
            let mut inner = state.inner.lock().await;
            let mut channel = channel_with_quality(&[0, 1], &[0, 1], None, None, &[]);
            let quality_state = channel.qualities.get_mut("720p").unwrap();
            quality_state
                .current_weights
                .insert("worker-a".to_owned(), 500);
            quality_state
                .current_weights
                .insert("worker-b".to_owned(), -500);
            inner.channels.insert("live".to_owned(), channel);
        }
        // 再登録すると選択の周期をやり直す
        state
            .register_worker("worker-a", "http://a.test", 1, 0)
            .await
            .unwrap();
        let inner = state.inner.lock().await;
        assert!(
            inner.channels["live"].qualities["720p"]
                .current_weights
                .is_empty()
        );
    }

    #[test]
    fn keeps_only_latest_playlist_range() {
        // 直近の範囲に収まるセグメント番号はそのまま残す
        let mut segments = BTreeSet::from([101, 102, 103, 104, 105, 106]);
        prune_segments(&mut segments, 6);
        assert_eq!(segments, BTreeSet::from([101, 102, 103, 104, 105, 106]));
        // 新しいセグメント番号が届いたら、範囲より古いセグメント番号は残さない
        segments.insert(110);
        prune_segments(&mut segments, 6);
        assert_eq!(segments, BTreeSet::from([105, 106, 110]));
    }

    #[test]
    fn keeps_encryption_for_playlist_and_worker_cache() {
        let key = segment_encryption("https://keys.test/a");
        let mut encryption = BTreeMap::from([
            (99, key.clone()),
            (100, key.clone()),
            (101, key.clone()),
            (102, key.clone()),
        ]);
        // メタデータに残る範囲と、worker が公開に使える範囲の指定だけを残す
        prune_encryption(
            &mut encryption,
            &BTreeSet::from([101, 102]),
            &BTreeSet::from([100]),
        );
        assert_eq!(
            encryption,
            BTreeMap::from([(100, key.clone()), (101, key.clone()), (102, key)])
        );
    }

    #[test]
    fn does_not_publish_before_due() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(&[0, 1, 2, 3, 4, 5], &[], Some(2_000), None, &[]),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5]),
        )]);
        advance_published(&mut channels, &workers, 6, 10_000, 1_999);
        let quality = &channels["live"].qualities["720p"];
        assert!(quality.published.is_empty());
        assert!(quality.allowed_through.is_none());
        assert_eq!(quality.publish_due_ms, Some(2_000));

        advance_published(&mut channels, &workers, 6, 10_000, 2_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.allowed_through, Some(5));
        assert_eq!(quality.published, BTreeSet::from([0, 1, 2, 3, 4, 5]));
    }

    #[test]
    fn publishes_newest_contiguous_range() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(
                &[0, 1, 2, 3, 4, 5, 6, 7, 8],
                &[0, 1, 2, 3, 4, 5],
                None,
                Some(8),
                &[],
            ),
        )]);
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_cached("live", "720p", &[3, 4, 5, 6, 7, 8]),
            ),
        ]);
        advance_published(&mut channels, &workers, 6, 10_000, 5_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.published, BTreeSet::from([3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn keeps_current_range_when_candidate_is_not_newer() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(&[0, 1, 2, 3, 4, 5], &[0, 1, 2, 3, 4, 5], None, Some(5), &[]),
        )]);
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_cached("live", "720p", &[2, 3, 4, 5]),
            ),
        ]);
        advance_published(&mut channels, &workers, 6, 10_000, 5_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.published, BTreeSet::from([0, 1, 2, 3, 4, 5]));
    }

    #[test]
    fn falls_back_when_current_range_is_not_served() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                &[6, 7, 8, 9, 10],
                None,
                Some(10),
                &[],
            ),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5]),
        )]);
        advance_published(&mut channels, &workers, 6, 10_000, 5_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.published, BTreeSet::from([0, 1, 2, 3, 4, 5]));
    }

    #[test]
    fn excludes_pending_segments() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(
                &[0, 1, 2, 3, 4, 5, 6, 7, 8],
                &[],
                Some(10_000),
                Some(5),
                &[],
            ),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5, 6, 7, 8]),
        )]);
        advance_published(&mut channels, &workers, 6, 10_000, 5_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.published, BTreeSet::from([0, 1, 2, 3, 4, 5]));
        assert_eq!(quality.allowed_through, Some(5));
    }

    #[test]
    fn advances_allowed_after_due() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(
                &[0, 1, 2, 3, 4, 5, 6, 7, 8],
                &[],
                Some(10_000),
                Some(5),
                &[],
            ),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5, 6, 7, 8]),
        )]);
        advance_published(&mut channels, &workers, 6, 10_000, 10_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.allowed_through, Some(8));
        assert_eq!(quality.published, BTreeSet::from([3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn finalizes_expired_wait_before_new_segment() {
        let mut channel = channel_with_quality(&[0, 1, 2, 3, 4, 5], &[], Some(1_000), None, &[]);
        let quality = channel.qualities.get_mut("720p").unwrap();
        advance_allowed(quality, 999);
        assert!(quality.allowed_through.is_none());
        assert_eq!(quality.publish_due_ms, Some(1_000));

        advance_allowed(quality, 1_000);
        assert_eq!(quality.allowed_through, Some(5));
        assert_eq!(quality.publish_due_ms, None);
    }

    #[test]
    fn backfill_starts_wait() {
        let mut channel = channel_with_quality(&[0, 2], &[], None, Some(2), &[]);
        let quality = channel.qualities.get_mut("720p").unwrap();
        register_segment_metadata(quality, 1, 1_000, 300, false);
        assert_eq!(quality.publish_due_ms, Some(1_300));
        assert!(quality.pending.contains(&1));

        advance_allowed(quality, 1_300);
        assert_eq!(quality.allowed_through, Some(2));
        assert!(!quality.pending.contains(&1));
    }

    #[test]
    fn known_segment_reput_does_not_start_wait() {
        let mut channel =
            channel_with_quality(&[1, 2, 3, 4, 5, 6], &[0, 1, 2, 3, 4, 5], None, Some(6), &[]);
        let quality = channel.qualities.get_mut("720p").unwrap();
        register_segment_metadata(quality, 0, 1_000, 300, true);
        assert_eq!(quality.publish_due_ms, None);
        assert!(!quality.pending.contains(&0));
    }

    #[test]
    fn keeps_pruned_segment_after_due() {
        // 0 はメタデータから prune 済みでも pending に残っており、期限後に公開可能になる
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(
                &[1, 2, 3, 4, 5, 6],
                &[],
                Some(1_000),
                None,
                &[0, 1, 2, 3, 4, 5, 6],
            ),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0]),
        )]);
        advance_published(&mut channels, &workers, 6, 10_000, 1_000);
        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.allowed_through, Some(6));
        assert_eq!(quality.published, BTreeSet::from([0]));
    }

    #[test]
    fn quality_ranges_are_independent() {
        let mut channel = ChannelState::new(6000, 0);
        add_quality(
            &mut channel,
            "480p",
            &[100, 101, 102],
            &[],
            None,
            Some(102),
            &[],
        );
        add_quality(&mut channel, "720p", &[0, 1, 2], &[], None, Some(2), &[]);
        let mut channels = BTreeMap::from([("live".to_owned(), channel)]);
        let workers = BTreeMap::from([
            (
                "worker-a".to_owned(),
                worker_with_cached("live", "720p", &[0, 1, 2]),
            ),
            (
                "worker-b".to_owned(),
                worker_with_cached("live", "480p", &[100, 101, 102]),
            ),
        ]);
        advance_published(&mut channels, &workers, 6, 10_000, 5_000);
        let channel = &channels["live"];
        assert_eq!(
            channel.qualities["720p"].published,
            BTreeSet::from([0, 1, 2])
        );
        assert_eq!(
            channel.qualities["480p"].published,
            BTreeSet::from([100, 101, 102])
        );
    }

    #[test]
    fn dead_worker_is_not_used() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(&[0, 1, 2, 3, 4, 5], &[], None, Some(5), &[]),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5]),
        )]);
        // last_sync_ms は 0 なので now=2,500、timeout=1,000 では配信先に使わない
        advance_published(&mut channels, &workers, 6, 1_000, 2_500);
        let quality = &channels["live"].qualities["720p"];
        assert!(quality.published.is_empty());
    }

    #[test]
    fn splits_cached_runs_at_gaps() {
        let cached = worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5, 7, 8]).cached;
        let ranges = worker_held_ranges("live", "720p", &cached, Some(8), &BTreeSet::new());
        assert_eq!(ranges, vec![(7, 8), (0, 5)]);
    }
}
