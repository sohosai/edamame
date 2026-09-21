//! HLS プレイリストの範囲計算と描画

use std::collections::{BTreeMap, BTreeSet};

use crate::error::EdamameError;

/// 1 つの quality のメディアプレイリスト
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    pub media_sequence: u64,
    pub target_duration_s: u64,
    pub segment_duration_ms: u64,
    pub sequences: Vec<u64>,
}

/// マスタープレイリストの 1 エントリ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterQuality {
    pub quality: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    pub range: (u64, u64),
}

/// マスタープレイリストを組み立てるための quality 入力
#[derive(Debug, Clone)]
pub struct QualityPlaylistInput {
    pub bandwidth: u64,
    pub resolution: Option<String>,
    pub segments: BTreeSet<u64>,
}

/// セグメントに適用する暗号化の指定
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentEncryption {
    /// 適用する鍵 URI
    pub key_uri: String,
    /// 適用する IV（16 byte の hex）。`None` なら `#EXT-X-KEY` に `IV` 属性を出さない
    pub iv: Option<String>,
}

/// 直近の範囲から連続したメディアプレイリストを計算する
pub fn plan_media_playlist(
    segments: &BTreeSet<u64>,
    segment_duration_ms: u64,
    playlist_segments: u64,
) -> Result<MediaPlaylist, EdamameError> {
    if playlist_segments == 0 {
        return Err(EdamameError::Internal(
            "playlist segments must be positive".to_owned(),
        ));
    }
    let latest = segments
        .iter()
        .next_back()
        .copied()
        .ok_or_else(|| EdamameError::Unprocessable("quality has no segments".to_owned()))?;
    let start = latest.saturating_sub(playlist_segments.saturating_sub(1));
    let first = segments
        .range(start..=latest)
        .next()
        .copied()
        .ok_or_else(|| EdamameError::Unprocessable("playlist range is empty".to_owned()))?;
    let expected = latest - first + 1;
    let actual = segments.range(first..=latest).count() as u64;
    if actual != expected {
        return Err(EdamameError::Unprocessable(
            "playlist range has a gap".to_owned(),
        ));
    }
    Ok(MediaPlaylist {
        media_sequence: first,
        target_duration_s: segment_duration_ms.div_ceil(1000).max(1),
        segment_duration_ms,
        sequences: (first..=latest).collect(),
    })
}

/// quality ごとの範囲を持つマスタープレイリストを計算する
pub fn plan_master_playlist(
    qualities: &BTreeMap<String, QualityPlaylistInput>,
    segment_duration_ms: u64,
    playlist_segments: u64,
) -> Result<Vec<MasterQuality>, EdamameError> {
    if qualities.is_empty() {
        return Err(EdamameError::NotFound(
            "channel has no qualities".to_owned(),
        ));
    }
    let mut entries = Vec::new();
    for (quality, input) in qualities {
        let playlist =
            plan_media_playlist(&input.segments, segment_duration_ms, playlist_segments)?;
        let last = *playlist
            .sequences
            .last()
            .ok_or_else(|| EdamameError::Unprocessable("playlist is empty".to_owned()))?;
        let range = (playlist.media_sequence, last);
        entries.push(MasterQuality {
            quality: quality.clone(),
            bandwidth: input.bandwidth,
            resolution: input.resolution.clone(),
            range,
        });
    }
    Ok(entries)
}

/// メディアプレイリストを文字列にする
#[must_use]
pub fn render_media_playlist(
    playlist: &MediaPlaylist,
    channel_id: &str,
    quality: &str,
    public_url: &str,
    encryption: &BTreeMap<u64, SegmentEncryption>,
) -> String {
    let mut output = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
    output.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        playlist.target_duration_s
    ));
    output.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        playlist.media_sequence
    ));
    let duration = format!(
        "{}.{:03}",
        playlist.segment_duration_ms / 1000,
        playlist.segment_duration_ms % 1000
    );
    let mut encrypted = false;
    for sequence in &playlist.sequences {
        match encryption.get(sequence) {
            // IV はセグメントごとに変わるため、暗号化されたセグメントには毎回 `#EXT-X-KEY` を出す
            Some(setting) => {
                output.push_str(&key_tag(setting));
                encrypted = true;
            }
            None => {
                if encrypted {
                    output.push_str("#EXT-X-KEY:METHOD=NONE\n");
                    encrypted = false;
                }
            }
        }
        output.push_str(&format!("#EXTINF:{duration},\n"));
        output.push_str(&format!(
            "{public_url}/segments/{channel_id}/{quality}/{sequence}.ts\n"
        ));
    }
    output
}

/// セグメントに適用する暗号化の指定を `#EXT-X-KEY` にする
fn key_tag(setting: &SegmentEncryption) -> String {
    let key_uri = &setting.key_uri;
    match setting.iv.as_deref() {
        Some(iv) => format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\",IV=0x{iv}\n"),
        None => format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n"),
    }
}

/// マスタープレイリストを文字列にする
#[must_use]
pub fn render_master_playlist(entries: &[MasterQuality]) -> String {
    let mut output = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
    for entry in entries {
        output.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={}", entry.bandwidth));
        if let Some(resolution) = &entry.resolution {
            output.push_str(&format!(",RESOLUTION={resolution}"));
        }
        output.push('\n');
        output.push_str(&format!("{}.m3u8\n", entry.quality));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(values: &[u64]) -> BTreeSet<u64> {
        values.iter().copied().collect()
    }

    /// 鍵 URI だけを指定する
    fn key(key_uri: &str) -> SegmentEncryption {
        SegmentEncryption {
            key_uri: key_uri.to_owned(),
            iv: None,
        }
    }

    /// 鍵 URI と IV を指定する
    fn key_with_iv(key_uri: &str, iv: &str) -> SegmentEncryption {
        SegmentEncryption {
            key_uri: key_uri.to_owned(),
            iv: Some(iv.to_owned()),
        }
    }

    #[test]
    fn plans_contiguous_media_playlist() {
        let playlist = plan_media_playlist(&segments(&[101, 102, 103]), 6000, 6).unwrap();
        assert_eq!(playlist.media_sequence, 101);
        assert_eq!(playlist.sequences, vec![101, 102, 103]);
        assert_eq!(playlist.target_duration_s, 6);
    }

    #[test]
    fn allows_short_playlist_at_start() {
        let playlist = plan_media_playlist(&segments(&[101, 102]), 6000, 6).unwrap();
        assert_eq!(playlist.sequences, vec![101, 102]);
    }

    #[test]
    fn rejects_gap() {
        assert!(plan_media_playlist(&segments(&[101, 102, 104, 105]), 6000, 6).is_err());
    }

    #[test]
    fn renders_media_playlist() {
        let playlist = plan_media_playlist(&segments(&[101, 102]), 6000, 6).unwrap();
        let rendered = render_media_playlist(
            &playlist,
            "live",
            "720p",
            "http://worker.test",
            &BTreeMap::new(),
        );
        assert!(rendered.contains("#EXT-X-TARGETDURATION:6"));
        assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:101"));
        assert!(rendered.contains("#EXTINF:6.000,"));
        assert!(rendered.contains("http://worker.test/segments/live/720p/102.ts"));
        assert!(!rendered.contains("#EXT-X-KEY"));
    }

    #[test]
    fn renders_a_key_tag_for_each_encrypted_segment() {
        let playlist = plan_media_playlist(&segments(&[101, 102, 103, 104]), 6000, 6).unwrap();
        let encryption = BTreeMap::from([
            (101, key("https://keys.test/a")),
            (102, key("https://keys.test/a")),
            (103, key("https://keys.test/b")),
        ]);
        let rendered =
            render_media_playlist(&playlist, "live", "720p", "http://worker.test", &encryption);
        // 鍵 URI が同じでも、暗号化されたセグメントには毎回タグを出す
        assert_eq!(
            rendered.matches("#EXT-X-KEY:METHOD=AES-128").count(),
            3,
            "暗号化されたセグメントの数だけタグを出す: {rendered}"
        );
        for (tag, segment) in [
            (
                "#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/a\"\n",
                101,
            ),
            (
                "#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/a\"\n",
                102,
            ),
            (
                "#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/b\"\n",
                103,
            ),
        ] {
            let tag_position = rendered.find(tag).expect("鍵のタグがない");
            let segment_position = rendered
                .find(&format!("/{segment}.ts\n"))
                .expect("セグメントがない");
            assert!(tag_position < segment_position);
        }
        // 暗号化されていないセグメントに戻るところで解除する
        let unencrypted = rendered.find("#EXT-X-KEY:METHOD=NONE\n").unwrap();
        let segment_104 = rendered.find("/104.ts\n").unwrap();
        assert!(unencrypted < segment_104);
    }

    #[test]
    fn renders_iv_for_each_segment() {
        let playlist = plan_media_playlist(&segments(&[101, 102, 103]), 6000, 6).unwrap();
        let key_uri = "https://keys.test/a";
        let encryption = BTreeMap::from([
            (
                101,
                key_with_iv(key_uri, "00000000000000000000000000000000"),
            ),
            (
                102,
                key_with_iv(key_uri, "00000000000000000000000000000001"),
            ),
            (
                103,
                key_with_iv(key_uri, "00000000000000000000000000000002"),
            ),
        ]);
        let rendered =
            render_media_playlist(&playlist, "live", "720p", "http://worker.test", &encryption);
        // 鍵 URI が同じでも IV が違うため、セグメントごとにタグを出す
        assert_eq!(rendered.matches("#EXT-X-KEY").count(), 3);
        for (sequence, iv) in [(101, 0), (102, 1), (103, 2)] {
            let tag = rendered
                .find(&format!(
                    "#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/a\",IV=0x{iv:032}\n"
                ))
                .unwrap();
            let segment = rendered.find(&format!("/{sequence}.ts\n")).unwrap();
            assert!(tag < segment);
        }
    }

    #[test]
    fn renders_master_playlist() {
        let entries = vec![MasterQuality {
            quality: "720p".to_owned(),
            bandwidth: 3_000_000,
            resolution: Some("1280x720".to_owned()),
            range: (101, 102),
        }];
        let rendered = render_master_playlist(&entries);
        assert!(rendered.contains("#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720"));
        assert!(rendered.contains("720p.m3u8"));
    }

    #[test]
    fn allows_qualities_with_different_ranges() {
        let qualities = BTreeMap::from([
            (
                "480p".to_owned(),
                QualityPlaylistInput {
                    bandwidth: 1_000_000,
                    resolution: None,
                    segments: segments(&[201, 202, 203]),
                },
            ),
            (
                "720p".to_owned(),
                QualityPlaylistInput {
                    bandwidth: 3_000_000,
                    resolution: None,
                    segments: segments(&[101, 102, 103]),
                },
            ),
        ]);
        let entries = plan_master_playlist(&qualities, 6000, 6).unwrap();
        assert_eq!(entries[0].range, (201, 203));
        assert_eq!(entries[1].range, (101, 103));
    }
}
