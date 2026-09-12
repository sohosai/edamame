//! worker の状態と、時刻を入力にした評価

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::segment::SegmentId;

/// worker の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerStateKind {
    Preparing,
    Ready,
    Dead,
}

/// 最後の sync と保持状況から worker の状態を評価する
#[must_use]
pub fn evaluate_worker_state(
    last_sync_ms: u64,
    now_ms: u64,
    sync_timeout_ms: u64,
    cached: &BTreeSet<SegmentId>,
    cache_target: &BTreeSet<SegmentId>,
) -> WorkerStateKind {
    if now_ms.saturating_sub(last_sync_ms) > sync_timeout_ms {
        return WorkerStateKind::Dead;
    }
    if cache_target.is_subset(cached) {
        WorkerStateKind::Ready
    } else {
        WorkerStateKind::Preparing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(sequences: &[u64]) -> BTreeSet<SegmentId> {
        sequences
            .iter()
            .map(|sequence| SegmentId {
                channel_id: "live".to_owned(),
                quality: "720p".to_owned(),
                sequence: *sequence,
            })
            .collect()
    }

    #[test]
    fn fresh_worker_with_all_segments_is_ready() {
        let cached = ids(&[101, 102]);
        let target = ids(&[101, 102]);
        assert_eq!(
            evaluate_worker_state(1_000, 1_000, 10_000, &cached, &target),
            WorkerStateKind::Ready
        );
    }

    #[test]
    fn fresh_worker_missing_segment_is_preparing() {
        let cached = ids(&[101]);
        let target = ids(&[101, 102]);
        assert_eq!(
            evaluate_worker_state(1_000, 1_000, 10_000, &cached, &target),
            WorkerStateKind::Preparing
        );
    }

    #[test]
    fn stale_worker_is_dead() {
        let cached = ids(&[101, 102]);
        let target = ids(&[101, 102]);
        assert_eq!(
            evaluate_worker_state(1_000, 11_001, 10_000, &cached, &target),
            WorkerStateKind::Dead
        );
    }
}
