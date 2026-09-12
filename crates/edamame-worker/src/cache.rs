//! worker のローカルキャッシュ。走査と索引を担当する。
//!
//! 索引の mutex をファイル I/O の間で保持しないため、ファイルの読み書きは
//! 呼び出し側で行う。sync の heartbeat が保存待ちで止まらないようにするためである。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use edamame_domain::{
    EdamameError, SegmentId, SegmentKey, parse_sequence_filename, render_filename,
    validate_identifier, validate_quality,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `(channel_id, quality)` ごとにシーケンス番号を保持するキャッシュ索引
#[derive(Debug)]
pub struct Cache {
    dir: PathBuf,
    limit: usize,
    entries: BTreeMap<(String, String), BTreeSet<u64>>,
}

impl Cache {
    /// `cache_dir` を走査してキャッシュ索引を作る
    pub async fn scan(dir: PathBuf, limit: u64) -> Result<Self, EdamameError> {
        let limit = usize::try_from(limit)
            .map_err(|_| EdamameError::BadRequest("cache limit is too large".to_owned()))?;
        let mut cache = Self {
            dir,
            limit,
            entries: BTreeMap::new(),
        };
        cache.scan_dir().await?;
        for path in cache.remove_over_limit() {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                eprintln!(
                    "failed to remove cached segment over the limit {}: {error}",
                    path.display()
                );
            }
        }
        Ok(cache)
    }

    /// セグメントを保持しているか
    #[must_use]
    pub fn contains(&self, channel_id: &str, quality: &str, sequence: u64) -> bool {
        self.entries
            .get(&(channel_id.to_owned(), quality.to_owned()))
            .is_some_and(|sequences| sequences.contains(&sequence))
    }

    /// セグメントファイルのパスを検証して返す
    pub fn path_for(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
    ) -> Result<PathBuf, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        validate_quality(quality)?;
        Ok(self.path(channel_id, quality, sequence))
    }

    /// 索引に追加し、キャッシュ上限を超えたファイルのパスを返す
    pub fn insert(
        &mut self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
    ) -> Result<Vec<PathBuf>, EdamameError> {
        validate_identifier("channel_id", channel_id)?;
        validate_quality(quality)?;
        self.entries
            .entry((channel_id.to_owned(), quality.to_owned()))
            .or_default()
            .insert(sequence);
        Ok(self.remove_over_limit())
    }

    /// 索引からセグメントを取り除く
    pub fn remove(&mut self, channel_id: &str, quality: &str, sequence: u64) {
        let key = (channel_id.to_owned(), quality.to_owned());
        if let Some(sequences) = self.entries.get_mut(&key) {
            sequences.remove(&sequence);
            if sequences.is_empty() {
                self.entries.remove(&key);
            }
        }
    }

    /// 索引から `(channel_id, quality)` のセグメントをすべて取り除き、削除するファイルのパスを返す
    pub fn clear(&mut self, channel_id: &str, quality: &str) -> Vec<PathBuf> {
        let Some(sequences) = self
            .entries
            .remove(&(channel_id.to_owned(), quality.to_owned()))
        else {
            return Vec::new();
        };
        sequences
            .iter()
            .map(|sequence| self.path(channel_id, quality, *sequence))
            .collect()
    }

    /// 索引にあるセグメントをすべて取り除き、削除するファイルのパスを返す
    pub fn clear_all(&mut self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for ((channel_id, quality), sequences) in &self.entries {
            for sequence in sequences {
                paths.push(self.path(channel_id, quality, *sequence));
            }
        }
        self.entries.clear();
        paths
    }

    /// sync に送るキャッシュ一覧
    #[must_use]
    pub fn keys(&self) -> Vec<SegmentKey> {
        let mut keys = Vec::new();
        for ((channel_id, quality), sequences) in &self.entries {
            for sequence in sequences {
                keys.push(SegmentKey {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    sequence: *sequence,
                });
            }
        }
        keys
    }

    /// 保持しているセグメントの識別子
    #[must_use]
    pub fn cached_ids(&self) -> BTreeSet<SegmentId> {
        let mut ids = BTreeSet::new();
        for ((channel_id, quality), sequences) in &self.entries {
            for sequence in sequences {
                ids.insert(SegmentId {
                    channel_id: channel_id.clone(),
                    quality: quality.clone(),
                    sequence: *sequence,
                });
            }
        }
        ids
    }

    async fn scan_dir(&mut self) -> Result<(), EdamameError> {
        let mut channels = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|error| EdamameError::Internal(format!("read cache dir failed: {error}")))?;
        while let Some(channel_entry) = channels
            .next_entry()
            .await
            .map_err(|error| EdamameError::Internal(format!("read cache dir failed: {error}")))?
        {
            if !channel_entry
                .file_type()
                .await
                .map_err(|error| EdamameError::Internal(format!("read file type failed: {error}")))?
                .is_dir()
            {
                continue;
            }
            let Some(channel_id) = channel_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_identifier("channel_id", &channel_id).is_err() {
                continue;
            }
            let mut qualities =
                tokio::fs::read_dir(channel_entry.path())
                    .await
                    .map_err(|error| {
                        EdamameError::Internal(format!("read quality dir failed: {error}"))
                    })?;
            while let Some(quality_entry) = qualities.next_entry().await.map_err(|error| {
                EdamameError::Internal(format!("read quality dir failed: {error}"))
            })? {
                if !quality_entry
                    .file_type()
                    .await
                    .map_err(|error| {
                        EdamameError::Internal(format!("read file type failed: {error}"))
                    })?
                    .is_dir()
                {
                    continue;
                }
                let Some(quality) = quality_entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if validate_quality(&quality).is_err() {
                    continue;
                }
                let mut files =
                    tokio::fs::read_dir(quality_entry.path())
                        .await
                        .map_err(|error| {
                            EdamameError::Internal(format!("read cache file dir failed: {error}"))
                        })?;
                while let Some(file_entry) = files.next_entry().await.map_err(|error| {
                    EdamameError::Internal(format!("read cache file dir failed: {error}"))
                })? {
                    if !file_entry
                        .file_type()
                        .await
                        .map_err(|error| {
                            EdamameError::Internal(format!("read file type failed: {error}"))
                        })?
                        .is_file()
                    {
                        continue;
                    }
                    let Some(filename) = file_entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Ok(sequence) = parse_sequence_filename(&filename) else {
                        continue;
                    };
                    self.entries
                        .entry((channel_id.clone(), quality.clone()))
                        .or_default()
                        .insert(sequence);
                }
            }
        }
        Ok(())
    }

    /// キャッシュ上限を超えたセグメントを索引から外し、削除するファイルのパスを返す
    fn remove_over_limit(&mut self) -> Vec<PathBuf> {
        let dir = self.dir.clone();
        let limit = self.limit;
        let mut over_limit = Vec::new();
        for ((channel_id, quality), sequences) in &mut self.entries {
            while sequences.len() > limit {
                let Some(sequence) = sequences.pop_first() else {
                    break;
                };
                over_limit.push(
                    dir.join(channel_id)
                        .join(quality)
                        .join(render_filename(sequence)),
                );
            }
        }
        over_limit
    }

    fn path(&self, channel_id: &str, quality: &str, sequence: u64) -> PathBuf {
        self.dir
            .join(channel_id)
            .join(quality)
            .join(render_filename(sequence))
    }
}

/// 一時ファイルに書いてから rename する
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), EdamameError> {
    let parent = path
        .parent()
        .ok_or_else(|| EdamameError::Internal("cache path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| EdamameError::Internal(format!("create cache dir failed: {error}")))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("segment");
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = tokio::fs::write(&temp_path, bytes).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(EdamameError::Internal(format!(
            "write cached segment failed: {error}"
        )));
    }
    if let Err(error) = tokio::fs::rename(&temp_path, path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(EdamameError::Internal(format!(
            "commit cached segment failed: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "edamame-worker-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    async fn store(
        cache: &mut Cache,
        channel_id: &str,
        quality: &str,
        sequence: u64,
        bytes: &[u8],
    ) {
        let path = cache.path_for(channel_id, quality, sequence).unwrap();
        write_atomic(&path, bytes).await.unwrap();
        for path in cache.insert(channel_id, quality, sequence).unwrap() {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    #[tokio::test]
    async fn scans_valid_files_and_ignores_others() {
        let dir = temp_dir("scan");
        std::fs::create_dir_all(dir.join("live/720p")).unwrap();
        std::fs::write(dir.join("live/720p/101.ts"), b"101").unwrap();
        std::fs::write(dir.join("live/720p/102.ts"), b"102").unwrap();
        std::fs::write(dir.join("live/720p/notes.txt"), b"notes").unwrap();
        std::fs::write(dir.join("live/720p/001.ts"), b"old").unwrap();
        let cache = Cache::scan(dir.clone(), 12).await.unwrap();
        assert!(cache.contains("live", "720p", 101));
        assert!(cache.contains("live", "720p", 102));
        assert!(dir.join("live/720p/notes.txt").exists());
        assert!(dir.join("live/720p/001.ts").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn removes_oldest_files_beyond_limit() {
        let dir = temp_dir("over-limit");
        let mut cache = Cache::scan(dir.clone(), 2).await.unwrap();
        for sequence in [101, 102, 103] {
            store(
                &mut cache,
                "live",
                "720p",
                sequence,
                format!("{sequence}").as_bytes(),
            )
            .await;
        }
        assert!(!cache.contains("live", "720p", 101));
        assert!(cache.contains("live", "720p", 102));
        assert!(cache.contains("live", "720p", 103));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn remove_deletes_index_entry() {
        let dir = temp_dir("remove");
        let mut cache = Cache::scan(dir.clone(), 2).await.unwrap();
        cache.insert("live", "720p", 101).unwrap();
        assert!(cache.contains("live", "720p", 101));
        cache.remove("live", "720p", 101);
        assert!(!cache.contains("live", "720p", 101));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn clear_removes_group_entries() {
        let dir = temp_dir("clear");
        let mut cache = Cache::scan(dir.clone(), 12).await.unwrap();
        cache.insert("live", "720p", 101).unwrap();
        cache.insert("live", "720p", 102).unwrap();
        cache.insert("live", "1080p", 101).unwrap();
        let cleared = cache.clear("live", "720p");
        assert_eq!(cleared.len(), 2);
        assert!(!cache.contains("live", "720p", 101));
        assert!(!cache.contains("live", "720p", 102));
        assert!(cache.contains("live", "1080p", 101));
        let _ = std::fs::remove_dir_all(dir);
    }
}
