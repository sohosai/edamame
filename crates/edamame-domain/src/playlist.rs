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
    for sequence in &playlist.sequences {
        output.push_str(&format!("#EXTINF:{duration},\n"));
        output.push_str(&format!(
            "{public_url}/segments/{channel_id}/{quality}/{sequence}.ts\n"
        ));
    }
    output
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
        let rendered = render_media_playlist(&playlist, "live", "720p", "http://worker.test");
        assert!(rendered.contains("#EXT-X-TARGETDURATION:6"));
        assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:101"));
        assert!(rendered.contains("#EXTINF:6.000,"));
        assert!(rendered.contains("http://worker.test/segments/live/720p/102.ts"));
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
