//! 再起動をまたいで残った ImageFlux チャンネルを追跡する

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 永続化する状態
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// 作成したが削除を確認できていない ImageFlux のチャンネル
    #[serde(default)]
    pub imageflux_channel_ids: Vec<String>,
}

impl State {
    /// 状態ファイルの既定の場所
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("com", "sohosai", "tomyou")
            .context("状態ディレクトリを決められません")?;
        Ok(dirs.data_local_dir().join("state.json"))
    }

    /// 状態を読む。ファイルがない場合は空の状態を返す。
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("状態ファイルを読めません: {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("状態ファイルを読めません: {}", path.display()))
            }
        }
    }

    /// 状態を保存する
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("状態ディレクトリを作れません: {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("状態を JSON にできません")?;
        fs::write(path, text)
            .with_context(|| format!("状態ファイルを書けません: {}", path.display()))
    }

    /// 状態ファイルを消す
    pub fn clear(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("状態ファイルを消せません: {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty_state() {
        let path = std::env::temp_dir().join("tomyou-state-does-not-exist.json");
        let _ = fs::remove_file(&path);
        let state = State::load(&path).expect("状態を読める");
        assert!(state.imageflux_channel_ids.is_empty());
    }

    #[test]
    fn round_trips_state() {
        // スレッド名には `::` が含まれ、Windows ではファイル名に使えない
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("現在時刻を取得できる")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tomyou-state-{}-{suffix}.json", std::process::id()));
        let state = State {
            imageflux_channel_ids: vec!["channel".to_owned()],
        };
        state.save(&path).expect("状態を保存できる");
        let loaded = State::load(&path).expect("状態を読める");
        assert_eq!(loaded.imageflux_channel_ids, vec!["channel"]);
        State::clear(&path).expect("状態を消せる");
    }
}
