//! controller のメモリ状態、インジェスト、registry、playlist 生成

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use edamame_domain::{
    EdamameError, QualityPlaylistInput, SegmentId, SegmentKey, SyncResponse, WorkerStateKind,
    evaluate_worker_state, normalize_http_url, plan_master_playlist, plan_media_playlist,
    render_master_playlist, render_media_playlist, validate_identifier, validate_quality,
    validate_resolution,
};
use tokio::sync::Mutex;

use crate::storage::CanonicalStore;

/// controller の共有状態
pub struct ControllerState {
    pub store: CanonicalStore,
    pub playlist_segments: u64,
    pub sync_interval_ms: u64,
    pub sync_timeout_ms: u64,
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
    pub segments: BTreeSet<u64>,
}

/// worker の登録内容と最後の報告
pub struct WorkerState {
    pub public_url: String,
    pub state: WorkerStateKind,
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
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            playlist_segments,
            sync_interval_ms,
            sync_timeout_ms,
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
                segments: BTreeSet::new(),
            });
        if quality_state.resolution.is_none() {
            quality_state.resolution = resolution;
        }
        quality_state.segments.insert(sequence);
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
            worker.state = WorkerStateKind::Preparing;
            Ok(false)
        } else {
            inner.workers.insert(
                worker_id.to_owned(),
                WorkerState {
                    public_url,
                    state: WorkerStateKind::Preparing,
                    last_sync_ms: now_ms,
                    cached: BTreeSet::new(),
                },
            );
            Ok(true)
        }
    }

    /// sync を受け付けて状態と取得一覧を返す
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
        let target = cache_target(&inner.channels, self.playlist_segments);
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| EdamameError::NotFound("worker is not registered".to_owned()))?;
        worker.last_sync_ms = now_ms;
        worker.cached = cached;
        worker.state = evaluate_worker_state(
            worker.last_sync_ms,
            now_ms,
            self.sync_timeout_ms,
            &worker.cached,
            &target,
        );
        let fetch_segments = target
            .difference(&worker.cached)
            .map(SegmentKey::from)
            .collect();
        Ok(SyncResponse {
            state: worker.state,
            fetch_segments,
        })
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
        let (segment_duration_ms, segments, target) = {
            // ファイルの存在確認の間は mutex を保持しない。プレイリストと worker の評価には同じメタデータを使う。
            let inner = self.inner.lock().await;
            let channel = inner
                .channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let quality_state = channel
                .qualities
                .get(quality)
                .ok_or_else(|| EdamameError::NotFound("quality not found".to_owned()))?;
            (
                channel.segment_duration_ms,
                quality_state.segments.clone(),
                cache_target(&inner.channels, self.playlist_segments),
            )
        };
        let playlist = plan_media_playlist(&segments, segment_duration_ms, self.playlist_segments)?;
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
        let mut inner = self.inner.lock().await;
        evaluate_all_workers(&mut inner.workers, &target, self.sync_timeout_ms, now_ms);
        let worker = inner
            .workers
            .values()
            .find(|worker| worker.state == WorkerStateKind::Ready)
            .ok_or_else(|| EdamameError::Unavailable("no READY worker".to_owned()))?;
        Ok(render_media_playlist(
            &playlist,
            channel_id,
            quality,
            &worker.public_url,
        ))
    }

    /// マスタープレイリストを生成する
    pub async fn master_playlist(
        &self,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<String, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        let (segment_duration_ms, inputs, target) = {
            // ファイルの存在確認の間は mutex を保持しない。プレイリストと worker の評価には同じメタデータを使う。
            let inner = self.inner.lock().await;
            let channel = inner
                .channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let inputs = channel
                .qualities
                .iter()
                .map(|(quality, state)| {
                    (
                        quality.clone(),
                        QualityPlaylistInput {
                            bandwidth: state.bandwidth,
                            resolution: state.resolution.clone(),
                            segments: state.segments.clone(),
                        },
                    )
                })
                .collect();
            (
                channel.segment_duration_ms,
                inputs,
                cache_target(&inner.channels, self.playlist_segments),
            )
        };
        let entries = plan_master_playlist(&inputs, segment_duration_ms, self.playlist_segments)?;
        for entry in &entries {
            for sequence in entry.range.0..=entry.range.1 {
                if !self
                    .store
                    .segment_exists(channel_id, &entry.quality, sequence)
                    .await?
                {
                    return Err(EdamameError::Unprocessable(
                        "segment file is missing".to_owned(),
                    ));
                }
            }
        }
        let mut inner = self.inner.lock().await;
        evaluate_all_workers(&mut inner.workers, &target, self.sync_timeout_ms, now_ms);
        if !inner
            .workers
            .values()
            .any(|worker| worker.state == WorkerStateKind::Ready)
        {
            return Err(EdamameError::Unavailable("no READY worker".to_owned()));
        }
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

/// メディアプレイリストに入り得る直近の範囲だけを残す
fn prune_segments(segments: &mut BTreeSet<u64>, playlist_segments: u64) {
    let Some(latest) = segments.iter().next_back().copied() else {
        return;
    };
    let start = latest.saturating_sub(playlist_segments.saturating_sub(1));
    if start > 0 {
        *segments = segments.split_off(&start);
    }
}

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

fn evaluate_all_workers(
    workers: &mut BTreeMap<String, WorkerState>,
    target: &BTreeSet<SegmentId>,
    timeout_ms: u64,
    now_ms: u64,
) {
    for worker in workers.values_mut() {
        worker.state = evaluate_worker_state(
            worker.last_sync_ms,
            now_ms,
            timeout_ms,
            &worker.cached,
            target,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
