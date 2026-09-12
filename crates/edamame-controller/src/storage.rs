//! controller の正規セグメントストレージ

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use edamame_domain::{EdamameError, render_filename};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `storage_dir` をルートとする正規ストア
#[derive(Debug, Clone)]
pub struct CanonicalStore {
    root: PathBuf,
}

impl CanonicalStore {
    /// ルートを作る
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// セグメントファイルのパスを返す
    #[must_use]
    pub fn segment_path(&self, channel_id: &str, quality: &str, sequence: u64) -> PathBuf {
        self.root
            .join(channel_id)
            .join(quality)
            .join(render_filename(sequence))
    }

    /// セグメントファイルの存在を確認する
    pub async fn segment_exists(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
    ) -> Result<bool, EdamameError> {
        let path = self.segment_path(channel_id, quality, sequence);
        tokio::fs::try_exists(&path)
            .await
            .map_err(|error| EdamameError::Internal(format!("check segment failed: {error}")))
    }

    /// セグメントファイルを読み込む
    pub async fn read_segment(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
    ) -> Result<Vec<u8>, EdamameError> {
        let path = self.segment_path(channel_id, quality, sequence);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(EdamameError::NotFound("segment not found".to_owned()))
            }
            Err(error) => Err(EdamameError::Internal(format!(
                "read segment failed: {error}"
            ))),
        }
    }

    /// セグメントファイルをアトミックに保存する
    pub async fn write_segment(
        &self,
        channel_id: &str,
        quality: &str,
        sequence: u64,
        bytes: &[u8],
    ) -> Result<(), EdamameError> {
        let path = self.segment_path(channel_id, quality, sequence);
        write_atomic(&path, bytes).await
    }
}

/// 一時ファイルに書いてから rename することで、読み手に中途半端なファイルを見せない
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), EdamameError> {
    let parent = path
        .parent()
        .ok_or_else(|| EdamameError::Internal("segment path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| EdamameError::Internal(format!("create storage dir failed: {error}")))?;
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
            "write segment failed: {error}"
        )));
    }
    if let Err(error) = tokio::fs::rename(&temp_path, path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(EdamameError::Internal(format!(
            "commit segment failed: {error}"
        )));
    }
    Ok(())
}
