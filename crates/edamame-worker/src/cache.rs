//! worker のローカルキャッシュの索引とセグメントファイルを扱う。
//!
//! セグメントファイルはメモリ上に保存する。呼び出し側は索引を参照または変更する間だけ mutex を保持する。

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use edamame_domain::{EdamameError, SegmentId, SegmentKey, validate_identifier, validate_quality};

/// `(channel_id, quality)` ごとに直近のセグメントファイルを保持するキャッシュ
#[derive(Debug)]
pub struct Cache {
    limit: usize,
    entries: BTreeMap<(String, String), BTreeMap<u64, Bytes>>,
}

/// キャッシュしているセグメントの集計値
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub channel_id: String,
    pub quality: String,
    pub segments: usize,
    pub head_sequence: u64,
}

impl Cache {
    /// 空のキャッシュを作る
    pub fn new(limit: u64) -> Result<Self, EdamameError> {
        let limit = usize::try_from(limit)
            .map_err(|_| EdamameError::BadRequest("cache limit is too large".to_owned()))?;
        Ok(Self {
            limit,
            entries: BTreeMap::new(),
        })
    }

    /// 指定したセグメントが索引に含まれるかを返す
    #[must_use]
    pub fn contains(&self, channel_id: &str, quality: &str, sequence: u64) -> bool {
        self.entries
            .get(&(channel_id.to_owned(), quality.to_owned()))
            .is_some_and(|segments| segments.contains_key(&sequence))
    }

    /// キャッシュしているセグメントファイルを返す
    /// 参照カウンタを進めるだけなので、同時に配信してもバイト列は複製されない
    #[must_use]
    pub fn get(&self, channel_id: &str, quality: &str, sequence: u64) -> Option<Bytes> {
        self.entries
            .get(&(channel_id.to_owned(), quality.to_owned()))
            .and_then(|segments| segments.get(&sequence))
            .cloned()
    }

    /// セグメントファイルを保存し、キャッシュ上限を超えた分を削除して削除した一覧を返す
    pub fn put(
        &mut self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
        bytes: Bytes,
    ) -> Result<Vec<SegmentKey>, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        validate_quality(quality)?;
        let pair = (channel_id.to_owned(), quality.to_owned());
        let segments = self.entries.entry(pair).or_default();
        segments.insert(sequence, bytes);
        let mut removed = Vec::new();
        while segments.len() > self.limit {
            let Some((&oldest, _)) = segments.iter().next() else {
                break;
            };
            segments.remove(&oldest);
            removed.push(SegmentKey {
                channel_id: channel_id.to_owned(),
                quality: quality.to_owned(),
                sequence: oldest,
            });
        }
        Ok(removed)
    }

    /// 索引からセグメントを取り除く
    pub fn remove(&mut self, channel_id: &str, quality: &str, sequence: u64) {
        let pair = (channel_id.to_owned(), quality.to_owned());
        if let Some(segments) = self.entries.get_mut(&pair) {
            segments.remove(&sequence);
            if segments.is_empty() {
                self.entries.remove(&pair);
            }
        }
    }

    /// 索引から `(channel_id, quality)` のセグメントをすべて取り除く
    pub fn clear(&mut self, channel_id: &str, quality: &str) {
        self.entries
            .remove(&(channel_id.to_owned(), quality.to_owned()));
    }

    /// 索引にあるセグメントをすべて取り除く
    pub fn clear_all(&mut self) {
        self.entries.clear();
    }

    /// sync に送るキャッシュ一覧を返す
    #[must_use]
    pub fn keys(&self) -> Vec<SegmentKey> {
        let mut keys = Vec::new();
        for ((channel_id, quality), segments) in &self.entries {
            for sequence in segments.keys() {
                keys.push(SegmentKey {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    sequence: *sequence,
                });
            }
        }
        keys
    }

    /// キャッシュしているセグメントを `SegmentId` の集合で返す
    #[must_use]
    pub fn cached_ids(&self) -> BTreeSet<SegmentId> {
        let mut ids = BTreeSet::new();
        for ((channel_id, quality), segments) in &self.entries {
            for sequence in segments.keys() {
                ids.insert(SegmentId {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    sequence: *sequence,
                });
            }
        }
        ids
    }

    /// `(channel_id, quality)` ごとのセグメント数と最新 sequence を返す
    #[must_use]
    pub fn stats(&self) -> Vec<CacheStats> {
        self.entries
            .iter()
            .filter_map(|((channel_id, quality), segments)| {
                let head_sequence = *segments.keys().next_back()?;
                Some(CacheStats {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    segments: segments.len(),
                    head_sequence,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(limit: u64) -> Cache {
        Cache::new(limit).unwrap()
    }

    #[test]
    fn stores_and_reads_segments() {
        let mut cache = cache(12);
        cache
            .put("live", "720p", 101, Bytes::from_static(b"segment-101"))
            .unwrap();
        assert!(cache.contains("live", "720p", 101));
        assert_eq!(
            cache.get("live", "720p", 101).unwrap().as_ref(),
            b"segment-101"
        );
        assert!(!cache.contains("live", "720p", 102));
        assert!(cache.get("live", "720p", 102).is_none());
    }

    #[test]
    fn removes_oldest_segments_beyond_limit() {
        let mut cache = cache(2);
        for sequence in [101, 102, 103] {
            cache
                .put("live", "720p", sequence, Bytes::from(format!("{sequence}")))
                .unwrap();
        }
        assert!(!cache.contains("live", "720p", 101));
        assert!(cache.contains("live", "720p", 102));
        assert!(cache.contains("live", "720p", 103));
    }

    #[test]
    fn reports_segment_removed_by_its_own_insert() {
        let mut cache = cache(2);
        cache
            .put("live", "720p", 102, Bytes::from_static(b"102"))
            .unwrap();
        cache
            .put("live", "720p", 103, Bytes::from_static(b"103"))
            .unwrap();
        // 上限まで埋まった状態で古い番号を入れると、入れたセグメント自身が削除される
        let removed = cache
            .put("live", "720p", 101, Bytes::from_static(b"101"))
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].sequence, 101);
        assert!(!cache.contains("live", "720p", 101));
    }

    #[test]
    fn remove_deletes_index_entry() {
        let mut cache = cache(2);
        cache
            .put("live", "720p", 101, Bytes::from_static(b"101"))
            .unwrap();
        assert!(cache.contains("live", "720p", 101));
        cache.remove("live", "720p", 101);
        assert!(!cache.contains("live", "720p", 101));
    }

    #[test]
    fn clear_removes_group_entries() {
        let mut cache = cache(12);
        cache
            .put("live", "720p", 101, Bytes::from_static(b"101"))
            .unwrap();
        cache
            .put("live", "720p", 102, Bytes::from_static(b"102"))
            .unwrap();
        cache
            .put("live", "1080p", 101, Bytes::from_static(b"101"))
            .unwrap();
        cache.clear("live", "720p");
        assert!(!cache.contains("live", "720p", 101));
        assert!(!cache.contains("live", "720p", 102));
        assert!(cache.contains("live", "1080p", 101));
    }

    #[test]
    fn keys_and_cached_ids_list_all_segments() {
        let mut cache = cache(12);
        cache
            .put("live", "720p", 101, Bytes::from_static(b"101"))
            .unwrap();
        cache
            .put("live", "1080p", 101, Bytes::from_static(b"101"))
            .unwrap();
        assert_eq!(cache.keys().len(), 2);
        assert_eq!(cache.cached_ids().len(), 2);
        cache.clear_all();
        assert!(cache.keys().is_empty());
        assert!(cache.cached_ids().is_empty());
    }

    #[test]
    fn stats_reports_segment_count_and_head_sequence_per_quality() {
        let mut cache = cache(2);
        for sequence in [101, 102] {
            cache
                .put("live", "720p", sequence, Bytes::from_static(b"720p"))
                .unwrap();
        }
        cache
            .put("live", "1080p", 201, Bytes::from_static(b"1080p"))
            .unwrap();

        let mut stats = cache.stats();
        stats.sort_by(|left, right| left.quality.cmp(&right.quality));

        assert_eq!(
            stats,
            vec![
                CacheStats {
                    channel_id: "live".to_owned(),
                    quality: "1080p".to_owned(),
                    segments: 1,
                    head_sequence: 201,
                },
                CacheStats {
                    channel_id: "live".to_owned(),
                    quality: "720p".to_owned(),
                    segments: 2,
                    head_sequence: 102,
                },
            ]
        );
    }
}
