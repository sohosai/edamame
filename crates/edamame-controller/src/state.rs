//! controller のメモリ状態、インジェスト、registry、playlist 生成

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use edamame_domain::{
    EdamameError, QualityPlaylistInput, SegmentId, SegmentKey, SyncResponse, normalize_http_url,
    now_ms, plan_master_playlist, plan_media_playlist, render_master_playlist,
    render_media_playlist, validate_identifier, validate_quality, validate_resolution,
};
use tokio::sync::Mutex;

use crate::storage::CanonicalStore;

/// controller の共有状態
pub struct ControllerState {
    pub store: CanonicalStore,
    pub playlist_segments: u64,
    pub sync_interval_ms: u64,
    pub sync_timeout_ms: u64,
    pub publish_wait_ms: u64,
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
    pub last_sync_ms: u64,
    pub cached: BTreeSet<SegmentId>,
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
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
            publish_wait_ms,
            inner: Mutex::new(Inner::default()),
            write_lock: Mutex::new(()),
        })
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
                published: BTreeSet::new(),
                publish_due_ms: None,
                allowed_through: None,
                pending: BTreeSet::new(),
                segments: BTreeSet::new(),
            });
        if quality_state.resolution.is_none() {
            quality_state.resolution = resolution;
        }
        register_segment_metadata(
            quality_state,
            sequence,
            committed_at_ms,
            self.publish_wait_ms,
            known_identifier && stored_in_quality,
        );
        prune_segments(&mut quality_state.segments, self.playlist_segments);
        Ok(outcome)
    }

    /// worker を登録または更新する。新規なら `true`。
    pub async fn register_worker(
        &self,
        worker_id: &str,
        public_url: &str,
        now_ms: u64,
    ) -> Result<bool, EdamameError> {
        validate_identifier("worker_id", worker_id)?;
        let public_url = normalize_http_url(public_url)?;
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.workers.get_mut(worker_id) {
            worker.public_url = public_url;
            // worker は登録のたびにキャッシュを捨てるため、報告済みのキャッシュも無効にする
            worker.cached = BTreeSet::new();
            Ok(false)
        } else {
            inner.workers.insert(
                worker_id.to_owned(),
                WorkerState {
                    public_url,
                    last_sync_ms: now_ms,
                    cached: BTreeSet::new(),
                },
            );
            Ok(true)
        }
    }

    /// sync を受け付けて取得一覧を返す
    pub async fn sync_worker(
        &self,
        worker_id: &str,
        cached_segments: Vec<SegmentKey>,
        now_ms: u64,
    ) -> Result<SyncResponse, EdamameError> {
        validate_identifier("worker_id", worker_id)?;
        let mut cached = BTreeSet::new();
        for key in cached_segments {
            cached.insert(key.into_id()?);
        }
        let mut inner = self.inner.lock().await;
        {
            let worker = inner
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| EdamameError::NotFound("worker is not registered".to_owned()))?;
            worker.last_sync_ms = now_ms;
            worker.cached = cached;
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
            .get(worker_id)
            // 上の get_mut で存在を確認済みのため、ここでは見つからないことがない
            .expect("worker was checked above");
        let fetch_segments = target
            .difference(&worker.cached)
            .map(SegmentKey::from)
            .collect();
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
        let (segment_duration_ms, published, worker_url) = {
            let inner = self.inner.lock().await;
            let Inner { channels, workers } = &*inner;
            let channel = channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let quality_state = channel
                .qualities
                .get(quality)
                .ok_or_else(|| EdamameError::NotFound("quality not found".to_owned()))?;
            if quality_state.published.is_empty() {
                return Err(EdamameError::Unavailable(
                    "playlist is not published yet".to_owned(),
                ));
            }
            let worker_url = workers
                .values()
                .find(|worker| {
                    is_alive(worker, now_ms, self.sync_timeout_ms)
                        && holds_segments(worker, channel_id, quality, &quality_state.published)
                })
                .map(|worker| worker.public_url.clone());
            (
                channel.segment_duration_ms,
                quality_state.published.clone(),
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
        ))
    }

    /// マスタープレイリストを生成する
    pub async fn master_playlist(
        &self,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<String, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        // ファイル確認の間は mutex を保持せず、公開範囲と配信可否を先に集める
        let (segment_duration_ms, published) = {
            let inner = self.inner.lock().await;
            let Inner { channels, workers } = &*inner;
            let channel = channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let mut published = BTreeMap::new();
            for (quality, quality_state) in &channel.qualities {
                if quality_state.published.is_empty() {
                    continue;
                }
                let served = workers.values().any(|worker| {
                    is_alive(worker, now_ms, self.sync_timeout_ms)
                        && holds_segments(worker, channel_id, quality, &quality_state.published)
                });
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
            if published.is_empty() {
                return Err(EdamameError::Unavailable(
                    "playlist is not published yet".to_owned(),
                ));
            }
            (channel.segment_duration_ms, published)
        };
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
        Ok(render_master_playlist(&entries))
    }
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
            last_sync_ms: 0,
            cached,
        }
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
