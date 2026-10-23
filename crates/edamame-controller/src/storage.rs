//! controller の正規セグメントストレージ

use std::collections::BTreeMap;
use std::sync::Mutex;

use bytes::Bytes;
use edamame_domain::{EdamameError, SegmentKey};

/// セグメントファイルをメモリ上に保存する正規ストア
///
/// 保存しているバイト数の合計が容量を超えたときは、書き込んだ順に古いセグメントから削除する
pub struct CanonicalStore {
    capacity: u64,
    inner: Mutex<StoreInner>,
}

#[derive(Default)]
struct StoreInner {
    segments: BTreeMap<SegmentKey, StoredSegment>,
    /// 削除する順番を決めるための、書き込み順の索引
    order: BTreeMap<u64, SegmentKey>,
    used_bytes: u64,
    next_order: u64,
}

struct StoredSegment {
    bytes: Bytes,
    order: u64,
}

impl CanonicalStore {
    /// 空のストアを作る
    #[must_use]
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            inner: Mutex::new(StoreInner::default()),
        }
    }

    /// 降格時に古いセグメントが残らないよう、索引と使用量もまとめて初期化する。
    pub fn clear(&self) {
        *self.inner.lock().expect("store lock is not poisoned") = StoreInner::default();
    }

    /// セグメントファイルが保存されているかを返す
    #[must_use]
    pub fn contains(&self, channel_id: &str, quality: &str, sequence: u64) -> bool {
        let inner = self.inner.lock().expect("store lock is not poisoned");
        inner
            .segments
            .contains_key(&segment_key(channel_id, quality, sequence))
    }

    /// セグメントファイルを読み込む
    /// 参照カウンタを進めるだけなので、同時に取得してもバイト列は複製されない
    pub fn get(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
    ) -> Result<Bytes, EdamameError> {
        let inner = self.inner.lock().expect("store lock is not poisoned");
        match inner
            .segments
            .get(&segment_key(channel_id, quality, sequence))
        {
            Some(segment) => Ok(segment.bytes.clone()),
            None => Err(EdamameError::NotFound("segment not found".to_owned())),
        }
    }

    /// セグメントファイルを保存し、容量を超えた分を削除して削除した一覧を返す
    pub fn put(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
        bytes: &[u8],
    ) -> Result<Vec<SegmentKey>, EdamameError> {
        let byte_count = bytes.len() as u64;
        if byte_count > self.capacity {
            return Err(EdamameError::PayloadTooLarge(
                "segment is larger than the storage capacity".to_owned(),
            ));
        }
        let key = segment_key(channel_id, quality, sequence);
        let mut inner = self.inner.lock().expect("store lock is not poisoned");
        // 同じセグメントファイルを上書きするときは、古いバイト数と書き込み順を取り除く
        if let Some(previous) = inner.segments.remove(&key) {
            inner.used_bytes -= previous.bytes.len() as u64;
            inner.order.remove(&previous.order);
        }
        let order = inner.next_order;
        inner.next_order += 1;
        inner.used_bytes += byte_count;
        inner.segments.insert(
            key.clone(),
            StoredSegment {
                bytes: Bytes::copy_from_slice(bytes),
                order,
            },
        );
        inner.order.insert(order, key);

        let mut removed = Vec::new();
        while inner.used_bytes > self.capacity {
            let Some((&oldest_order, _)) = inner.order.iter().next() else {
                break;
            };
            let Some(oldest) = inner.order.remove(&oldest_order) else {
                break;
            };
            if let Some(segment) = inner.segments.remove(&oldest) {
                inner.used_bytes -= segment.bytes.len() as u64;
                removed.push(oldest);
            }
        }
        Ok(removed)
    }

    /// `(channel_id, quality)` に保存されているセグメント番号を昇順で返す
    #[must_use]
    pub fn sequences(&self, channel_id: &str, quality: &str) -> Vec<u64> {
        let inner = self.inner.lock().expect("store lock is not poisoned");
        inner
            .segments
            .keys()
            .filter(|key| key.channel_id == channel_id && key.quality == quality)
            .map(|key| key.sequence)
            .collect()
    }

    /// 保存に使っているバイト数を返す
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        let inner = self.inner.lock().expect("store lock is not poisoned");
        inner.used_bytes
    }
}

fn segment_key(channel_id: &str, quality: &str, sequence: u64) -> SegmentKey {
    SegmentKey {
        channel_id: channel_id.to_owned(),
        quality: quality.to_owned(),
        sequence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_segments() {
        let store = CanonicalStore::new(1024);
        assert!(!store.contains("live", "720p", 0));
        let removed = store.put("live", "720p", 0, b"segment-0").unwrap();
        assert!(removed.is_empty());
        assert!(store.contains("live", "720p", 0));
        assert_eq!(store.get("live", "720p", 0).unwrap().as_ref(), b"segment-0");
        assert_eq!(store.used_bytes(), 9);
        assert!(store.get("live", "720p", 1).is_err());
    }

    #[test]
    fn removes_oldest_segments_beyond_capacity() {
        let store = CanonicalStore::new(16);
        store.put("live", "720p", 0, b"00000000").unwrap();
        store.put("live", "720p", 1, b"11111111").unwrap();
        let removed = store.put("live", "720p", 2, b"22222222").unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].sequence, 0);
        assert!(!store.contains("live", "720p", 0));
        assert!(store.contains("live", "720p", 1));
        assert!(store.contains("live", "720p", 2));
        assert_eq!(store.used_bytes(), 16);
    }

    #[test]
    fn rejects_segments_larger_than_capacity() {
        let store = CanonicalStore::new(4);
        let error = store.put("live", "720p", 0, b"12345").unwrap_err();
        assert_eq!(error.status_code(), 413);
        assert!(!store.contains("live", "720p", 0));
        assert_eq!(store.used_bytes(), 0);
    }

    #[test]
    fn overwrite_does_not_count_bytes_twice() {
        let store = CanonicalStore::new(16);
        store.put("live", "720p", 0, b"00000000").unwrap();
        store.put("live", "720p", 0, b"1111").unwrap();
        assert_eq!(store.used_bytes(), 4);
    }

    #[test]
    fn lists_stored_sequences_by_quality() {
        let store = CanonicalStore::new(1024);
        store.put("live", "720p", 1, b"a").unwrap();
        store.put("live", "720p", 0, b"b").unwrap();
        store.put("live", "480p", 5, b"c").unwrap();
        assert_eq!(store.sequences("live", "720p"), vec![0, 1]);
        assert_eq!(store.sequences("live", "480p"), vec![5]);
        assert!(store.sequences("live", "1080p").is_empty());
    }
}
