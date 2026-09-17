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

/// このセグメント番号以降に適用する鍵 URI
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChange {
    pub from_sequence: u64,
    /// 適用する鍵 URI。`None` なら暗号化しない
    pub key_uri: Option<String>,
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

/// セグメントの鍵 URI を変更履歴に記録する
///
/// 同じ識別子の再 PUT ではそのセグメントだけを置き換え、前後のセグメントの値は変えない。
/// 履歴は区間の開始点だけを持ち、直前の区間と同じ値になった開始点は残さない。
pub fn record_key_change(changes: &mut Vec<KeyChange>, sequence: u64, key_uri: Option<String>) {
    // 最近の開始点から遡り、そのセグメントに今適用されている値と差し込む位置を探す
    let mut index = changes.len();
    while index > 0 && changes[index - 1].from_sequence > sequence {
        index -= 1;
    }
    let previous = index
        .checked_sub(1)
        .map(|previous| changes[previous].key_uri.clone())
        .unwrap_or(None);
    if previous == key_uri {
        // 区間の値が変わらないため、開始点も変えない
        return;
    }
    // そのセグメントの開始点があれば置き換え、なければ番号順に差し込む
    let replace = index > 0 && changes[index - 1].from_sequence == sequence;
    let position = if replace {
        changes[index - 1].key_uri = key_uri;
        index - 1
    } else {
        changes.insert(
            index,
            KeyChange {
                from_sequence: sequence,
                key_uri,
            },
        );
        index
    };
    // そのセグメントより後ろが元の値のままになるよう、次の番号から元の値に戻す
    let next = sequence.checked_add(1);
    if let Some(next) = next
        && changes
            .get(position + 1)
            .is_none_or(|change| change.from_sequence != next)
    {
        changes.insert(
            position + 1,
            KeyChange {
                from_sequence: next,
                key_uri: previous,
            },
        );
    }
    // 直前の区間と同じ値になった開始点は残さない。先頭は暗号化なしから始まるため、先頭の暗号化なしも残さない。
    let previous_value = position
        .checked_sub(1)
        .map(|previous| changes[previous].key_uri.clone())
        .unwrap_or(None);
    let same_as_previous = previous_value == changes[position].key_uri;
    if same_as_previous {
        changes.remove(position);
    }
    // そのセグメントに適用する値。直前の開始点を消した場合は前の区間の値が引き継がれる
    let applied = if same_as_previous {
        previous_value
    } else {
        changes[position].key_uri.clone()
    };
    // 次の区間と同じ値になった開始点も残さない
    let next_position = if same_as_previous {
        position
    } else {
        position + 1
    };
    if let Some(next) = next
        && changes
            .get(next_position)
            .is_some_and(|change| change.from_sequence == next && change.key_uri == applied)
    {
        changes.remove(next_position);
    }
}

/// 指定したセグメント番号の集合を描画するのに必要な変更履歴だけを取り出す
///
/// 集合の先頭より前の最後の開始点と、集合の範囲内の開始点を残す。
#[must_use]
pub fn key_changes_for_segments(changes: &[KeyChange], segments: &BTreeSet<u64>) -> Vec<KeyChange> {
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        return Vec::new();
    };
    // 最近の開始点から遡り、集合の先頭より前の最後の開始点を探す
    let mut newer = changes.len();
    while newer > 0 && changes[newer - 1].from_sequence >= *first {
        newer -= 1;
    }
    let mut extracted = Vec::new();
    if newer > 0 {
        extracted.push(changes[newer - 1].clone());
    }
    for change in &changes[newer..] {
        if change.from_sequence > *last {
            break;
        }
        extracted.push(change.clone());
    }
    extracted
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
    key_changes: &[KeyChange],
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
    let mut cursor = 0;
    let mut previous_key_uri: Option<&str> = None;
    for sequence in &playlist.sequences {
        // 開始点は番号順なので、セグメント番号に合わせて位置を進める
        while key_changes
            .get(cursor + 1)
            .is_some_and(|change| change.from_sequence <= *sequence)
        {
            cursor += 1;
        }
        let key_uri = key_changes
            .get(cursor)
            .filter(|change| change.from_sequence <= *sequence)
            .and_then(|change| change.key_uri.as_deref());
        if key_uri != previous_key_uri {
            match key_uri {
                Some(key_uri) => {
                    output.push_str(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n"))
                }
                None => output.push_str("#EXT-X-KEY:METHOD=NONE\n"),
            }
        }
        previous_key_uri = key_uri;
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

    fn changes(entries: &[(u64, Option<&str>)]) -> Vec<KeyChange> {
        entries
            .iter()
            .map(|(from_sequence, key_uri)| KeyChange {
                from_sequence: *from_sequence,
                key_uri: key_uri.map(str::to_owned),
            })
            .collect()
    }

    /// 指定した範囲の各セグメントに適用する鍵 URI
    fn applied(changes: &[KeyChange], range: std::ops::RangeInclusive<u64>) -> Vec<Option<&str>> {
        range
            .map(|sequence| {
                changes
                    .iter()
                    .rev()
                    .find(|change| change.from_sequence <= sequence)
                    .and_then(|change| change.key_uri.as_deref())
            })
            .collect()
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
        let rendered = render_media_playlist(&playlist, "live", "720p", "http://worker.test", &[]);
        assert!(rendered.contains("#EXT-X-TARGETDURATION:6"));
        assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:101"));
        assert!(rendered.contains("#EXTINF:6.000,"));
        assert!(rendered.contains("http://worker.test/segments/live/720p/102.ts"));
        assert!(!rendered.contains("#EXT-X-KEY"));
    }

    #[test]
    fn keeps_only_key_changes() {
        let key_a = "https://keys.test/a";
        let key_b = "https://keys.test/b";
        let mut changes = Vec::new();
        // 先頭が暗号化なしなら記録しない
        record_key_change(&mut changes, 0, None);
        assert!(changes.is_empty());
        // 0 から 3 に A、B、B、A の順で鍵を記録する
        record_key_change(&mut changes, 0, Some(key_a.to_owned()));
        record_key_change(&mut changes, 1, Some(key_b.to_owned()));
        record_key_change(&mut changes, 2, Some(key_b.to_owned()));
        record_key_change(&mut changes, 3, Some(key_a.to_owned()));
        assert_eq!(
            applied(&changes, 0..=4),
            [Some(key_a), Some(key_b), Some(key_b), Some(key_a), None]
        );
        // 途中のセグメントの再 PUT はそのセグメントだけを置き換える
        record_key_change(&mut changes, 1, Some(key_a.to_owned()));
        assert_eq!(
            applied(&changes, 0..=4),
            [Some(key_a), Some(key_a), Some(key_b), Some(key_a), None]
        );
        // 同じ値が続くだけの配信では、履歴は変更の数だけしか増えない
        let mut constant = Vec::new();
        for sequence in 0..100 {
            record_key_change(&mut constant, sequence, Some(key_a.to_owned()));
        }
        assert_eq!(constant.len(), 2);
        assert!(
            applied(&constant, 0..=99)
                .iter()
                .all(|key_uri| *key_uri == Some(key_a))
        );
    }

    #[test]
    fn extracts_key_changes_in_range() {
        let key_a = "https://keys.test/a";
        let key_b = "https://keys.test/b";
        let mut changes = Vec::new();
        record_key_change(&mut changes, 4, Some(key_a.to_owned()));
        record_key_change(&mut changes, 6, None);
        record_key_change(&mut changes, 9, Some(key_b.to_owned()));
        let extracted = key_changes_for_segments(&changes, &segments(&[5, 6, 7, 8, 9, 10]));
        // 範囲内のすべてのセグメントで、元の履歴と同じ鍵 URI になる
        for sequence in 5..=10 {
            assert_eq!(
                applied(&extracted, sequence..=sequence),
                applied(&changes, sequence..=sequence)
            );
        }
        assert!(extracted.iter().all(|change| change.from_sequence <= 10));
    }

    #[test]
    fn renders_key_uri_changes() {
        let playlist = plan_media_playlist(&segments(&[101, 102, 103, 104]), 6000, 6).unwrap();
        let mut changes = Vec::new();
        record_key_change(&mut changes, 101, Some("https://keys.test/a".to_owned()));
        record_key_change(&mut changes, 102, Some("https://keys.test/a".to_owned()));
        record_key_change(&mut changes, 103, Some("https://keys.test/b".to_owned()));
        record_key_change(&mut changes, 104, None);
        let rendered =
            render_media_playlist(&playlist, "live", "720p", "http://worker.test", &changes);
        let key_a = rendered
            .find("#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/a\"")
            .unwrap();
        let segment_101 = rendered.find("/101.ts").unwrap();
        let key_b = rendered
            .find("#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.test/b\"")
            .unwrap();
        let segment_103 = rendered.find("/103.ts").unwrap();
        let unencrypted = rendered.find("#EXT-X-KEY:METHOD=NONE").unwrap();
        let segment_104 = rendered.find("/104.ts").unwrap();
        assert!(key_a < segment_101);
        assert!(segment_101 < key_b);
        assert!(key_b < segment_103);
        assert!(segment_103 < unencrypted);
        assert!(unencrypted < segment_104);
    }

    #[test]
    fn renders_key_uri_before_playlist_start() {
        // 変更が公開範囲より前にある場合も、先頭のセグメントに鍵を出す
        let playlist = plan_media_playlist(&segments(&[101, 102]), 6000, 6).unwrap();
        let changes = self::changes(&[(90, Some("https://keys.test/a"))]);
        let rendered =
            render_media_playlist(&playlist, "live", "720p", "http://worker.test", &changes);
        let key = rendered.find("#EXT-X-KEY:METHOD=AES-128").unwrap();
        let segment_101 = rendered.find("/101.ts").unwrap();
        assert!(key < segment_101);
        assert_eq!(rendered.matches("#EXT-X-KEY").count(), 1);
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
