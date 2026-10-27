//! controller のメモリ状態、セグメントの受け入れ、worker の登録、プレイリストの生成

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;

use edamame_domain::{
    EdamameError, QualityPlaylistInput, SegmentEncryption, SegmentId, SegmentKey, SyncResponse,
    normalize_external_m3u8, normalize_iv, normalize_key_uri, normalize_public_url, now_ms,
    plan_master_playlist, plan_media_playlist, render_master_playlist, render_media_playlist,
    validate_identifier, validate_quality, validate_resolution,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::external;
use crate::metrics::ControllerMetrics;
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
    pub(crate) metrics: ControllerMetrics,
    pub inner: Mutex<Inner>,
}

/// メモリ上の channel と worker
#[derive(Default)]
pub struct Inner {
    pub channels: BTreeMap<String, ChannelState>,
    pub workers: BTreeMap<String, WorkerState>,
    /// 同じセグメント番号に別のバイト列で保存し直したセグメント
    /// worker が古いバイト列をキャッシュしている可能性があるため、sync で再取得を指示する。
    /// セグメントファイルが保存されている間は記録を残し、公開範囲が古い範囲へ戻ったときにも指示する
    pub replaced: BTreeSet<SegmentId>,
}

/// channel のメタデータ
pub struct ChannelState {
    pub segment_duration_ms: u64,
    /// 最後に新しいセグメントを保存した時刻
    pub last_ingest_ms: u64,
    pub qualities: BTreeMap<String, QualityState>,
    /// 外部配信の master m3u8 の URL。publisher が PUT で宣言する
    pub external_m3u8: Option<String>,
    /// セグメント識別子から割り当てたセグメント番号への対応
    pub identifiers: BTreeMap<String, IdentifierState>,
    /// 新しいセグメント識別子に次回割り当てるセグメント番号
    pub next_sequence: u64,
}

/// セグメント識別子に割り当てたセグメント番号と、セグメントファイルを保存済みの quality の集合
pub struct IdentifierState {
    pub sequence: u64,
    pub qualities: BTreeSet<String>,
}

impl ChannelState {
    fn new(segment_duration_ms: u64, next_sequence: u64) -> Self {
        Self {
            segment_duration_ms,
            last_ingest_ms: 0,
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
    /// セグメント番号ごとの暗号化の指定
    /// 登録がないセグメントは暗号化されていない
    pub encryption: BTreeMap<u64, SegmentEncryption>,
    /// 保存されているセグメント番号から識別子を引く索引
    /// セグメントファイルを削除したときに、識別子の対応も一緒に外すために使う
    pub segment_identifiers: BTreeMap<u64, String>,
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
    /// 最後に新しいセグメントを保存した時刻
    pub last_ingest_ms: Option<u64>,
    /// 保存に成功したセグメント番号の最大値
    pub highest_ingested_sequence: Option<u64>,
    /// 停止後に再公開しないセグメント番号の上限
    pub publish_floor: Option<u64>,
}

/// worker の登録内容と最後の報告
pub struct WorkerState {
    pub public_url: String,
    /// 相対的な配信能力を表す重み
    pub weight: i64,
    pub last_sync_ms: u64,
    pub cached: BTreeSet<SegmentId>,
    /// worker が取得中と報告したセグメント
    /// 同じ番号に保存し直したときに、古いバイト列がキャッシュされるのを防ぐために使う
    pub fetching: BTreeSet<SegmentId>,
    /// 直前の sync 応答で取得を指示したセグメント
    /// 取得が終わって報告されるまでの間に、同じ番号に保存し直したことを検出するために使う
    pub instructed: BTreeSet<SegmentId>,
}

/// heartbeat で controller 間を共有する worker の状態
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub worker_id: String,
    pub public_url: String,
    pub weight: u32,
    pub cached_segments: Vec<SegmentKey>,
    pub fetching_segments: Vec<SegmentKey>,
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
            metrics: ControllerMetrics::new(),
            inner: Mutex::new(Inner::default()),
        })
    }

    /// 次の昇格で古い採番や登録を使わないよう、セグメントを含む配信状態を全消去する。
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        *inner = Inner::default();
        self.store.clear();
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
        // IV は鍵と組で使うため、暗号鍵の URI なしでは指定できない
        let iv = match iv {
            Some(value) => {
                if key_uri.is_none() {
                    return Err(EdamameError::BadRequest("iv requires key_uri".to_owned()));
                }
                Some(normalize_iv(value)?)
            }
            None => None,
        };

        // 保存とメタデータの更新を inner の mutex で直列化し、容量を超えて削除したセグメントを同じ時点で反映する
        let mut inner = self.inner.lock().await;
        validate_segment_metadata(
            &inner,
            channel_id,
            quality,
            duration_ms,
            bandwidth,
            resolution.as_deref(),
        )?;
        let (sequence, known_identifier, stored_in_quality, next_sequence) =
            match inner.channels.get(channel_id) {
                Some(channel) => match channel.identifiers.get(identifier) {
                    Some(state) => (
                        state.sequence,
                        true,
                        // 同じ quality に保存済みの場合だけ、バイト列を比較する
                        state.qualities.contains(quality),
                        None,
                    ),
                    None => (
                        channel.next_sequence,
                        false,
                        false,
                        Some(channel.next_sequence.checked_add(1).ok_or_else(|| {
                            EdamameError::Internal("segment sequence is exhausted".to_owned())
                        })?),
                    ),
                },
                // controller の起動ごとに 0 から採番する
                None => (0, false, false, Some(1)),
            };

        let outcome = if stored_in_quality {
            // quality の登録が残るのは、セグメントファイルが保存されている間だけ
            let existing = self
                .store
                .get(channel_id, quality, sequence)
                .map_err(|_| EdamameError::Internal("stored segment is missing".to_owned()))?;
            // 同じ識別子の再 PUT は、保存済みのバイト列と同じ場合だけ受け付ける
            if existing.as_ref() != bytes {
                return Err(EdamameError::Conflict(
                    "segment bytes differ from the canonical file".to_owned(),
                ));
            }
            IngestOutcome::AlreadyExists { sequence }
        } else {
            let removed = self.store.put(channel_id, quality, sequence, bytes)?;
            purge_removed_segments(&mut inner, &removed);
            // 識別子が別の quality に残っていて番号を使い回す場合、worker が古いバイト列をキャッシュしているか、取得中かもしれない
            if known_identifier {
                let id = SegmentId {
                    channel_id: channel_id.to_owned(),
                    quality: quality.to_owned(),
                    sequence,
                };
                let may_hold_old_bytes = inner.workers.values().any(|worker| {
                    worker.cached.contains(&id)
                        || worker.fetching.contains(&id)
                        || worker.instructed.contains(&id)
                });
                if may_hold_old_bytes {
                    inner.replaced.insert(id);
                }
            }
            IngestOutcome::Created { sequence }
        };

        // controller の保存が終わる前に公開待ちを始めると、worker が取得に使える時間が短くなる
        let committed_at_ms = now_ms();
        let created = matches!(outcome, IngestOutcome::Created { .. });
        let channel = inner
            .channels
            .entry(channel_id.to_owned())
            .or_insert_with(|| ChannelState::new(duration_ms, 1));
        if created {
            channel.last_ingest_ms = committed_at_ms;
        }
        let segment_duration_ms = channel.segment_duration_ms;
        let identifier_state = channel
            .identifiers
            .entry(identifier.to_owned())
            .or_insert_with(|| IdentifierState {
                sequence,
                qualities: BTreeSet::new(),
            });
        identifier_state.qualities.insert(quality.to_owned());
        if let Some(next_sequence) = next_sequence {
            channel.next_sequence = next_sequence;
        }
        let quality_state = channel
            .qualities
            .entry(quality.to_owned())
            .or_insert_with(|| QualityState {
                bandwidth,
                resolution: resolution.clone(),
                encryption: BTreeMap::new(),
                segment_identifiers: BTreeMap::new(),
                current_weights: BTreeMap::new(),
                published: BTreeSet::new(),
                publish_due_ms: None,
                allowed_through: None,
                pending: BTreeSet::new(),
                segments: BTreeSet::new(),
                last_ingest_ms: None,
                highest_ingested_sequence: None,
                publish_floor: None,
            });
        if created {
            let stale = quality_state.last_ingest_ms.is_some_and(|last| {
                committed_at_ms.saturating_sub(last)
                    > stale_after_ms(
                        segment_duration_ms,
                        self.playlist_segments,
                        self.publish_wait_ms,
                    )
            });
            if stale {
                if let Some(highest) = quality_state.highest_ingested_sequence {
                    quality_state.publish_floor = Some(
                        quality_state
                            .publish_floor
                            .map_or(highest, |floor| floor.max(highest)),
                    );
                }
                quality_state.published.clear();
                quality_state.current_weights.clear();
                quality_state.publish_due_ms = None;
                quality_state.pending.clear();
            }
            quality_state.last_ingest_ms = Some(committed_at_ms);
            quality_state.highest_ingested_sequence = Some(
                quality_state
                    .highest_ingested_sequence
                    .map_or(sequence, |highest| highest.max(sequence)),
            );
        }
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
        quality_state
            .segment_identifiers
            .insert(sequence, identifier.to_owned());
        prune_segments(&mut quality_state.segments, self.playlist_segments);
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
            // 再登録時は、controller が保持する報告済みのキャッシュと取得の指示を無効にする
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
        // 変更後の重みの比で選べるよう、全 quality の選択に使う現在値をリセットする
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
        let advances = {
            let Inner {
                channels, workers, ..
            } = &mut *inner;
            advance_published(
                channels,
                workers,
                self.playlist_segments,
                self.sync_timeout_ms,
                now_ms,
            )
        };
        for advance in advances {
            self.metrics
                .record_published(&advance.channel_id, &advance.quality, advance.segments);
        }
        let target = cache_target(&inner.channels, self.playlist_segments);
        // 同じ番号で保存し直したセグメントは、キャッシュの報告に関わらず取得を指示する。
        // 公開範囲はキャッシュ対象より古い番号を含むことがあり、後で古い範囲へ戻ることもある
        let keep: BTreeSet<SegmentId> = inner
            .replaced
            .iter()
            .filter(|id| is_in_playback_range(&inner, &target, id))
            .cloned()
            .collect();
        let worker = inner
            .workers
            .get_mut(worker_id)
            // 上の get_mut で存在を確認済みのため、ここでは見つからないことがない
            .expect("worker was checked above");
        let mut missing: BTreeSet<SegmentId> = target.difference(&worker.cached).cloned().collect();
        missing.extend(keep);
        let fetch_segments = missing.iter().map(SegmentKey::from).collect();
        // 取得中でまだ報告していないセグメントを、再保存のときに検出できるようにする
        worker.instructed = missing;
        Ok(SyncResponse { fetch_segments })
    }

    /// 他 controller の heartbeat に含まれる全 worker の状態を返す。
    pub async fn worker_snapshots(&self) -> Vec<WorkerSnapshot> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .map(|(worker_id, worker)| WorkerSnapshot {
                worker_id: worker_id.clone(),
                public_url: worker.public_url.clone(),
                weight: u32::try_from(worker.weight).unwrap_or(u32::MAX),
                cached_segments: worker.cached.iter().map(SegmentKey::from).collect(),
                fetching_segments: worker.fetching.iter().map(SegmentKey::from).collect(),
            })
            .collect()
    }

    /// 他 controller から受け取った worker の状態を反映する。
    /// worker が sync した controller だけが fetch 指示を返すため、instructed は消す。
    pub async fn apply_worker_snapshot(
        &self,
        snapshot: WorkerSnapshot,
        now_ms: u64,
    ) -> Result<(), EdamameError> {
        validate_identifier("worker_id", &snapshot.worker_id)?;
        if snapshot.weight == 0 {
            return Err(EdamameError::BadRequest(
                "weight must be positive".to_owned(),
            ));
        }
        let public_url = normalize_public_url(&snapshot.public_url)?;
        let mut cached = BTreeSet::new();
        for key in snapshot.cached_segments {
            cached.insert(key.into_id()?);
        }
        let mut fetching = BTreeSet::new();
        for key in snapshot.fetching_segments {
            fetching.insert(key.into_id()?);
        }

        let mut inner = self.inner.lock().await;
        let changed = if let Some(worker) = inner.workers.get_mut(&snapshot.worker_id) {
            let changed =
                worker.public_url != public_url || worker.weight != i64::from(snapshot.weight);
            worker.public_url = public_url;
            worker.weight = i64::from(snapshot.weight);
            worker.last_sync_ms = now_ms;
            worker.cached = cached;
            worker.fetching = fetching;
            worker.instructed.clear();
            changed
        } else {
            inner.workers.insert(
                snapshot.worker_id,
                WorkerState {
                    public_url,
                    weight: i64::from(snapshot.weight),
                    last_sync_ms: now_ms,
                    cached,
                    fetching,
                    instructed: BTreeSet::new(),
                },
            );
            true
        };
        if changed {
            reset_current_weights(&mut inner.channels);
        }
        let advances = {
            let Inner {
                channels, workers, ..
            } = &mut *inner;
            advance_published(
                channels,
                workers,
                self.playlist_segments,
                self.sync_timeout_ms,
                now_ms,
            )
        };
        for advance in advances {
            self.metrics
                .record_published(&advance.channel_id, &advance.quality, advance.segments);
        }
        Ok(())
    }

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
            let Inner {
                channels, workers, ..
            } = &mut *inner;
            let channel = channels
                .get_mut(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let segment_duration_ms = channel.segment_duration_ms;
            let quality_state = channel
                .qualities
                .get_mut(quality)
                .ok_or_else(|| EdamameError::NotFound("quality not found".to_owned()))?;
            if is_stale(
                quality_state,
                segment_duration_ms,
                self.playlist_segments,
                self.publish_wait_ms,
                now_ms,
            ) {
                return Err(EdamameError::Unavailable(
                    "publishing is stopped".to_owned(),
                ));
            }
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
                segment_duration_ms,
                quality_state.published.clone(),
                // 表は公開に使う範囲と worker のキャッシュの範囲に間引いてあるため、そのまま複製する
                quality_state.encryption.clone(),
                worker_url,
            )
        };
        let playlist =
            plan_media_playlist(&published, segment_duration_ms, self.playlist_segments)?;
        for sequence in &playlist.sequences {
            if !self.store.contains(channel_id, quality, *sequence) {
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
    /// 外部配信へ振り分けるかどうかは `routing` で指定する。
    /// 外部配信が登録されていなければ、振り分ける指定でも自前のマスタープレイリストを返す。
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
            let Inner {
                channels, workers, ..
            } = &*inner;
            let channel = channels
                .get(channel_id)
                .ok_or_else(|| EdamameError::NotFound("channel not found".to_owned()))?;
            let mut published = BTreeMap::new();
            let mut external_percent = 0;
            for (quality, quality_state) in &channel.qualities {
                if quality_state.published.is_empty() {
                    continue;
                }
                if is_stale(
                    quality_state,
                    channel.segment_duration_ms,
                    self.playlist_segments,
                    self.publish_wait_ms,
                    now_ms,
                ) {
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
            // 配信できる quality が 1 つもないときはすべて外部配信へ振り分ける
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
                if !self.store.contains(channel_id, quality, *sequence) {
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

    /// 現在の状態をメトリクスに記録する
    pub async fn record_metrics(&self, now_ms: u64) {
        let inner = self.inner.lock().await;
        self.metrics.record_up();
        for (worker_id, worker) in &inner.workers {
            self.metrics.record_worker(
                worker_id,
                age_seconds(now_ms, worker.last_sync_ms),
                worker.weight,
            );
        }
        for (channel_id, channel) in &inner.channels {
            self.metrics.record_channel(
                channel_id,
                age_seconds(now_ms, channel.last_ingest_ms),
                milliseconds_to_seconds(channel.segment_duration_ms),
                published_lag_seconds(channel, now_ms),
            );
        }
    }

    /// 5 秒ごとに状態をメトリクスへ記録するタスクを起動する
    #[must_use = "終了時に abort するため、JoinHandle を保持すること"]
    pub fn spawn_metrics_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                state.record_metrics(now_ms()).await;
            }
        })
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

/// 新しいセグメントが届かなくなってから、公開を停止するまでの猶予
fn stale_after_ms(segment_duration_ms: u64, playlist_segments: u64, publish_wait_ms: u64) -> u64 {
    segment_duration_ms
        .saturating_mul(playlist_segments)
        .saturating_add(publish_wait_ms)
}

/// 最後の新規セグメントから猶予を超えて公開が止まっているか
fn is_stale(
    quality: &QualityState,
    segment_duration_ms: u64,
    playlist_segments: u64,
    publish_wait_ms: u64,
    now_ms: u64,
) -> bool {
    let Some(last_ingest_ms) = quality.last_ingest_ms else {
        return false;
    };
    now_ms.saturating_sub(last_ingest_ms)
        > stale_after_ms(segment_duration_ms, playlist_segments, publish_wait_ms)
}

fn age_seconds(now_ms: u64, then_ms: u64) -> f64 {
    milliseconds_to_seconds(now_ms.saturating_sub(then_ms))
}

fn milliseconds_to_seconds(milliseconds: u64) -> f64 {
    milliseconds as f64 / 1000.0
}

/// 最後のセグメント到着から、公開済みの末尾までの遅れを秒で返す
///
/// まだ何も公開していない quality は、保持している先頭セグメントの 1 つ前まで
/// 公開済みとみなし、未公開のセグメント数を遅れに含める。
fn published_lag_seconds(channel: &ChannelState, now_ms: u64) -> f64 {
    let mut max_unpublished = 0;
    for quality_state in channel.qualities.values() {
        let Some(latest) = quality_state.segments.iter().next_back().copied() else {
            continue;
        };
        let published_tail = quality_state
            .published
            .iter()
            .next_back()
            .copied()
            .unwrap_or_else(|| {
                quality_state
                    .segments
                    .iter()
                    .next()
                    .copied()
                    .unwrap_or(latest)
                    .saturating_sub(1)
            });
        max_unpublished = max_unpublished.max(latest.saturating_sub(published_tail));
    }
    age_seconds(now_ms, channel.last_ingest_ms)
        + milliseconds_to_seconds(max_unpublished.saturating_mul(channel.segment_duration_ms))
}

/// 全 quality について、重み付きの選択に使う現在値をリセットする
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

/// 公開範囲が進んだセグメント
struct PublishedAdvance {
    channel_id: String,
    quality: String,
    segments: u64,
}

/// 各 quality の公開範囲を独立に進め、新しく公開したセグメントを返す
fn advance_published(
    channels: &mut BTreeMap<String, ChannelState>,
    workers: &BTreeMap<String, WorkerState>,
    playlist_segments: u64,
    sync_timeout_ms: u64,
    now_ms: u64,
) -> Vec<PublishedAdvance> {
    let mut advances = Vec::new();
    for (channel_id, channel) in channels.iter_mut() {
        for (quality, quality_state) in channel.qualities.iter_mut() {
            let before = quality_state.published.clone();
            advance_quality_published(
                channel_id,
                quality,
                quality_state,
                workers,
                playlist_segments,
                sync_timeout_ms,
                now_ms,
            );
            let newly_published = quality_state.published.difference(&before).count();
            if newly_published > 0 {
                advances.push(PublishedAdvance {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    segments: u64::try_from(newly_published).unwrap_or(u64::MAX),
                });
            }
        }
    }
    advances
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

/// 期限内に sync した worker が保持する連続範囲を、公開範囲の候補として新しい順に返す
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
            quality_state.publish_floor,
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
    publish_floor: Option<u64>,
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut first = None;
    let mut last = None;
    for id in cached.iter().rev() {
        if id.channel_id.as_str() != channel_id
            || id.quality.as_str() != quality
            || !is_allowed(allowed_through, pending, id.sequence)
            || publish_floor.is_some_and(|floor| id.sequence <= floor)
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

/// メタデータ上に残す直近のセグメントの範囲を計算する
fn prune_segments(segments: &mut BTreeSet<u64>, playlist_segments: u64) {
    let Some(latest) = segments.iter().next_back().copied() else {
        return;
    };
    let start = latest.saturating_sub(playlist_segments.saturating_sub(1));
    if start > 0 {
        *segments = segments.split_off(&start);
    }
}

/// 削除したセグメントのメタデータを、保存されているセグメントだけに揃える
fn purge_removed_segments(inner: &mut Inner, removed: &[SegmentKey]) {
    for key in removed {
        // 保存がなくなったセグメントに再取得の指示は要らない
        inner.replaced.remove(&SegmentId {
            channel_id: key.channel_id.clone(),
            quality: key.quality.clone(),
            sequence: key.sequence,
        });
        let Some(channel) = inner.channels.get_mut(&key.channel_id) else {
            continue;
        };
        // 保存がなくなったセグメントの暗号化、公開待ち、識別子の対応を残さない
        let identifier = {
            let Some(quality) = channel.qualities.get_mut(&key.quality) else {
                continue;
            };
            quality.segments.remove(&key.sequence);
            quality.encryption.remove(&key.sequence);
            quality.pending.remove(&key.sequence);
            quality.segment_identifiers.remove(&key.sequence)
        };
        // 別の quality に同じ識別子が残っている間は、識別子と番号の対応を残す
        let Some(identifier) = identifier else {
            continue;
        };
        if let Some(state) = channel.identifiers.get_mut(&identifier) {
            state.qualities.remove(&key.quality);
            if state.qualities.is_empty() {
                channel.identifiers.remove(&identifier);
            }
        }
    }
}

/// セグメントが worker のキャッシュ対象か、公開範囲に含まれるかを返す
/// 公開範囲は遅れた worker に合わせてキャッシュ対象より古い番号を含むことがある
fn is_in_playback_range(inner: &Inner, target: &BTreeSet<SegmentId>, id: &SegmentId) -> bool {
    if target.contains(id) {
        return true;
    }
    inner
        .channels
        .get(&id.channel_id)
        .and_then(|channel| channel.qualities.get(&id.quality))
        .is_some_and(|quality| quality.published.contains(&id.sequence))
}

/// worker のキャッシュ対象となる直近のセグメントの範囲を返す
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
            let start = latest
                .saturating_sub(playlist_segments.saturating_sub(1))
                .max(
                    quality_state
                        .publish_floor
                        .map_or(0, |floor| floor.saturating_add(1)),
                );
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
                encryption: BTreeMap::new(),
                segment_identifiers: BTreeMap::new(),
                current_weights: BTreeMap::new(),
                published: published.iter().copied().collect(),
                publish_due_ms,
                allowed_through,
                pending: pending.iter().copied().collect(),
                segments: segments.iter().copied().collect(),
                last_ingest_ms: Some(0),
                highest_ingested_sequence: None,
                publish_floor: None,
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

    #[test]
    fn published_lag_includes_ingest_age_and_unpublished_segments() {
        let mut channel = channel_with_quality(&[0, 1, 2, 3, 4, 5], &[0, 1, 2], None, Some(5), &[]);
        channel.last_ingest_ms = 1_000;

        let lag = published_lag_seconds(&channel, 4_000);

        assert_eq!(lag, 3.0 + 3.0 * 6.0);
    }

    #[test]
    fn published_lag_counts_all_retained_segments_when_nothing_is_published() {
        let mut channel = channel_with_quality(&[2, 3, 4], &[], None, Some(4), &[]);
        channel.last_ingest_ms = 2_000;

        let lag = published_lag_seconds(&channel, 5_000);

        // 保持している 2, 3, 4 がすべて未公開なので、3 セグメント分を含める
        assert_eq!(lag, 3.0 + 3.0 * 6.0);
    }

    #[test]
    fn advance_published_reports_newly_published_segments() {
        let mut channels = BTreeMap::from([(
            "live".to_owned(),
            channel_with_quality(&[0, 1, 2], &[], None, Some(2), &[]),
        )]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2]),
        )]);

        let advances = advance_published(&mut channels, &workers, 6, 10_000, 5_000);

        assert_eq!(advances.len(), 1);
        assert_eq!(advances[0].channel_id, "live");
        assert_eq!(advances[0].quality, "720p");
        assert_eq!(advances[0].segments, 3);
    }

    #[tokio::test]
    async fn last_ingest_advances_only_for_new_segments() {
        let state = ControllerState::new(CanonicalStore::new(1_000), 6, 1_000, 10_000, 0, 1);
        ingest_encrypted_segment(&state, 0).await;
        let first = state.inner.lock().await.channels["live"].last_ingest_ms;
        assert!(first > 0);

        // 同じ識別子・同じバイト列の再 PUT では最終到着時刻を進めない
        {
            let mut inner = state.inner.lock().await;
            inner.channels.get_mut("live").unwrap().last_ingest_ms = 1;
        }
        ingest_encrypted_segment(&state, 0).await;

        assert_eq!(state.inner.lock().await.channels["live"].last_ingest_ms, 1);
    }

    /// 保存されているセグメントの暗号化の指定は、メタデータの window から外れても残す
    #[tokio::test]
    async fn keeps_encryption_of_stored_segments_outside_window() {
        let state = ControllerState::new(CanonicalStore::new(1_000), 6, 1_000, 10_000, 0, 1);
        for sequence in 0..18 {
            ingest_encrypted_segment(&state, sequence).await;
        }
        let inner = state.inner.lock().await;
        let quality = &inner.channels["live"].qualities["720p"];
        assert!(quality.segments.contains(&17));
        assert!(!quality.segments.contains(&0));
        assert!(
            quality.encryption.contains_key(&0),
            "保存されているセグメントの暗号化の指定が失われている"
        );
    }

    /// 保存容量を超えたセグメントのメタデータを残さない
    #[tokio::test]
    async fn purge_metadata_of_removed_segments() {
        // セグメントは 7 byte なので、容量 14 では直近の 2 つだけを保存する
        let state = ControllerState::new(CanonicalStore::new(14), 6, 1_000, 10_000, 0, 1);
        for sequence in 0..6 {
            ingest_encrypted_segment(&state, sequence).await;
        }
        assert_eq!(state.store.sequences("live", "720p"), vec![4, 5]);
        {
            let inner = state.inner.lock().await;
            let quality = &inner.channels["live"].qualities["720p"];
            assert_eq!(
                quality.encryption.keys().copied().collect::<Vec<_>>(),
                vec![4, 5]
            );
            assert!(!quality.segment_identifiers.contains_key(&0));
            assert!(!inner.channels["live"].identifiers.contains_key("seg-0"));
        }
        // 削除された識別子の再 PUT は、新しい番号を割り当てて保存する
        let outcome = state
            .ingest_segment(SegmentIngest {
                channel_id: "live",
                quality: "720p",
                identifier: "seg-0",
                duration_ms: 6000,
                bandwidth: 3_000_000,
                resolution: None,
                key_uri: Some("https://keys.test/a"),
                iv: Some("00000000000000000000000000000000"),
                bytes: b"segment",
            })
            .await
            .unwrap();
        assert_eq!(outcome, IngestOutcome::Created { sequence: 6 });
    }

    /// 別の quality に同じ識別子が残っている間は、同じ番号で再保存する
    #[tokio::test]
    async fn keeps_sequence_while_identifier_stays_in_another_quality() {
        let state = ControllerState::new(CanonicalStore::new(14), 6, 1_000, 10_000, 0, 1);
        for quality in ["720p", "480p"] {
            state
                .ingest_segment(SegmentIngest {
                    channel_id: "live",
                    quality,
                    identifier: "seg-0",
                    duration_ms: 6000,
                    bandwidth: 3_000_000,
                    resolution: None,
                    key_uri: None,
                    iv: None,
                    bytes: b"segment",
                })
                .await
                .unwrap();
        }
        // 720p の次のセグメントで、720p の seg-0 が容量を超えて削除される
        state
            .ingest_segment(SegmentIngest {
                channel_id: "live",
                quality: "720p",
                identifier: "seg-1",
                duration_ms: 6000,
                bandwidth: 3_000_000,
                resolution: None,
                key_uri: None,
                iv: None,
                bytes: b"segment",
            })
            .await
            .unwrap();
        {
            let inner = state.inner.lock().await;
            let state = &inner.channels["live"].identifiers["seg-0"];
            assert_eq!(state.sequence, 0);
            assert!(state.qualities.contains("480p"));
            assert!(!state.qualities.contains("720p"));
        }
        // 480p に識別子が残っているため、720p への再 PUT は同じ番号 0 に保存する
        let outcome = state
            .ingest_segment(SegmentIngest {
                channel_id: "live",
                quality: "720p",
                identifier: "seg-0",
                duration_ms: 6000,
                bandwidth: 3_000_000,
                resolution: None,
                key_uri: None,
                iv: None,
                bytes: b"segment",
            })
            .await
            .unwrap();
        assert_eq!(outcome, IngestOutcome::Created { sequence: 0 });
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
        let state = ControllerState::new(CanonicalStore::new(1_024), 6, 1_000, 10_000, 0, 1);
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
        // 直近のセグメントの範囲に収まるセグメント番号はそのまま残す
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
        let ranges = worker_held_ranges("live", "720p", &cached, Some(8), &BTreeSet::new(), None);
        assert_eq!(ranges, vec![(7, 8), (0, 5)]);
    }

    #[test]
    fn detects_stale_publication_at_the_grace_boundary() {
        let mut channel =
            channel_with_quality(&[0, 1, 2, 3, 4, 5], &[0, 1, 2, 3, 4, 5], None, Some(5), &[]);
        let quality = channel.qualities.get_mut("720p").unwrap();
        quality.last_ingest_ms = Some(10_000);

        // 6000 ms * 6 + 2000 ms = 38000 ms
        assert!(!is_stale(quality, 6000, 6, 2000, 48_000));
        assert!(is_stale(quality, 6000, 6, 2000, 48_001));
        // 時計が戻った場合は失効させない
        assert!(!is_stale(quality, 6000, 6, 2000, 9_000));
    }

    #[tokio::test]
    async fn media_playlist_returns_unavailable_when_stale() {
        let state = ControllerState::new(CanonicalStore::new(1_000), 6, 1_000, 10_000, 0, 1);
        {
            let mut inner = state.inner.lock().await;
            let mut channel =
                channel_with_quality(&[0, 1, 2, 3, 4, 5], &[0, 1, 2, 3, 4, 5], None, Some(5), &[]);
            channel.qualities.get_mut("720p").unwrap().last_ingest_ms = Some(1);
            inner.channels.insert("live".to_owned(), channel);
        }

        let error = state
            .media_playlist("live", "720p", 36_002)
            .await
            .expect_err("停止中はプレイリストを返さない");
        assert!(matches!(error, EdamameError::Unavailable(_)));
    }

    #[tokio::test]
    async fn master_playlist_excludes_only_stale_quality() {
        let state = ControllerState::new(CanonicalStore::new(1_000), 6, 1_000, 10_000, 0, 1);
        let mut channel =
            channel_with_quality(&[0, 1, 2, 3, 4, 5], &[0, 1, 2, 3, 4, 5], None, Some(5), &[]);
        channel.qualities.get_mut("720p").unwrap().last_ingest_ms = Some(40_000);
        add_quality(
            &mut channel,
            "1080p",
            &[0, 1, 2, 3, 4, 5],
            &[0, 1, 2, 3, 4, 5],
            None,
            Some(5),
            &[],
        );
        channel.qualities.get_mut("1080p").unwrap().last_ingest_ms = Some(1);
        {
            let mut inner = state.inner.lock().await;
            inner.channels.insert("live".to_owned(), channel);
            let mut worker = worker_with_cached_qualities(&[
                ("live", "720p", &[0, 1, 2, 3, 4, 5]),
                ("live", "1080p", &[0, 1, 2, 3, 4, 5]),
            ]);
            worker.last_sync_ms = 40_000;
            inner.workers.insert("worker-a".to_owned(), worker);
        }
        for sequence in 0..6 {
            state.store.put("live", "720p", sequence, b"720p").unwrap();
        }

        let response = state
            .master_playlist("live", 40_000, ExternalRouting::LocalOnly)
            .await
            .unwrap();
        let MasterResponse::Local(body) = response else {
            panic!("自前のマスタープレイリストを期待した");
        };
        assert!(body.contains("720p.m3u8"));
        assert!(!body.contains("1080p.m3u8"));

        // すべて停止した場合は master も 503 相当になる
        {
            let mut inner = state.inner.lock().await;
            inner
                .channels
                .get_mut("live")
                .unwrap()
                .qualities
                .get_mut("720p")
                .unwrap()
                .last_ingest_ms = Some(1);
        }
        let error = match state
            .master_playlist("live", 40_000, ExternalRouting::LocalOnly)
            .await
        {
            Ok(_) => panic!("全 quality 停止中は master を返さない"),
            Err(error) => error,
        };
        assert!(matches!(error, EdamameError::Unavailable(_)));
    }

    #[tokio::test]
    async fn long_stop_sets_publish_floor_and_clears_publication() {
        let state = ControllerState::new(CanonicalStore::new(1_000), 6, 1_000, 10_000, 0, 1);
        ingest_encrypted_segment(&state, 0).await;
        {
            let mut inner = state.inner.lock().await;
            let quality = inner
                .channels
                .get_mut("live")
                .unwrap()
                .qualities
                .get_mut("720p")
                .unwrap();
            quality.last_ingest_ms = Some(1);
            quality.published = BTreeSet::from([0]);
        }

        ingest_encrypted_segment(&state, 1).await;

        let inner = state.inner.lock().await;
        let quality = &inner.channels["live"].qualities["720p"];
        assert_eq!(quality.publish_floor, Some(0));
        assert!(quality.published.is_empty());
        assert_eq!(quality.highest_ingested_sequence, Some(1));
        assert!(quality.publish_due_ms.is_some());
    }

    #[test]
    fn resumed_publication_starts_after_the_floor() {
        let mut channel = channel_with_quality(&[0, 1, 2, 3, 4, 5, 6], &[], None, Some(6), &[]);
        let quality = channel.qualities.get_mut("720p").unwrap();
        quality.last_ingest_ms = Some(6_000);
        quality.publish_floor = Some(5);
        let mut channels = BTreeMap::from([("live".to_owned(), channel)]);
        let workers = BTreeMap::from([(
            "worker-a".to_owned(),
            worker_with_cached("live", "720p", &[0, 1, 2, 3, 4, 5, 6]),
        )]);

        advance_published(&mut channels, &workers, 6, 10_000, 6_000);

        let quality = &channels["live"].qualities["720p"];
        assert_eq!(quality.published, BTreeSet::from([6]));
    }

    #[test]
    fn cache_target_skips_segments_at_or_before_floor() {
        let mut channel = channel_with_quality(&[0, 1, 2, 3, 4, 5], &[], None, Some(5), &[]);
        channel.qualities.get_mut("720p").unwrap().publish_floor = Some(3);
        let channels = BTreeMap::from([("live".to_owned(), channel)]);

        let target = cache_target(&channels, 6);

        assert!(target.iter().all(|id| id.sequence > 3));
        assert!(target.iter().any(|id| id.sequence == 4));
        assert!(target.iter().any(|id| id.sequence == 5));
    }
}
