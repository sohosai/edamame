//! publisher の OpenTelemetry メトリクス

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Gauge};

/// publisher が記録するメトリクスのハンドル
pub(crate) struct PublisherMetrics {
    up: Gauge<i64>,
    segments_encoded: Counter<u64>,
    segments_published: Counter<u64>,
    upload_errors: Counter<u64>,
    queue_depth: Gauge<u64>,
}

impl PublisherMetrics {
    pub(crate) fn new() -> Self {
        let meter = global::meter("edamame-publisher");
        Self {
            up: meter.i64_gauge("edamame.publisher.up").build(),
            segments_encoded: meter
                .u64_counter("edamame.publisher.segments.encoded")
                .build(),
            segments_published: meter
                .u64_counter("edamame.publisher.segments.published")
                .build(),
            upload_errors: meter.u64_counter("edamame.publisher.upload.errors").build(),
            queue_depth: meter.u64_gauge("edamame.publisher.queue.depth").build(),
        }
    }

    pub(crate) fn record_up(&self) {
        self.up.record(1, &[]);
    }

    pub(crate) fn record_down(&self) {
        self.up.record(0, &[]);
    }

    pub(crate) fn record_encoded(&self, channel_id: &str, quality: &str) {
        self.segments_encoded.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
            ],
        );
    }

    pub(crate) fn record_published(&self, channel_id: &str, quality: &str) {
        self.segments_published.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
            ],
        );
    }

    pub(crate) fn record_upload_error(&self, channel_id: &str, quality: &str, kind: &'static str) {
        self.upload_errors.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
                KeyValue::new("kind", kind),
            ],
        );
    }

    pub(crate) fn record_queue_depth(&self, channel_id: &str, depth: usize) {
        self.queue_depth.record(
            depth as u64,
            &[KeyValue::new("channel_id", channel_id.to_owned())],
        );
    }
}
