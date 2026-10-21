//! worker の OpenTelemetry メトリクス

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Gauge};

/// worker が記録するメトリクスのハンドル
pub(crate) struct WorkerMetrics {
    up: Gauge<i64>,
    controller_last_success_age: Gauge<f64>,
    cache_head_sequence: Gauge<u64>,
    cache_segments: Gauge<u64>,
    segments_downloaded: Counter<u64>,
    segments_served: Counter<u64>,
    errors: Counter<u64>,
}

impl WorkerMetrics {
    pub(crate) fn new() -> Self {
        let meter = global::meter("edamame-worker");
        Self {
            up: meter.i64_gauge("edamame.worker.up").build(),
            controller_last_success_age: meter
                .f64_gauge("edamame.worker.controller.last_success_age")
                .with_unit("s")
                .build(),
            cache_head_sequence: meter
                .u64_gauge("edamame.worker.cache.head_sequence")
                .build(),
            cache_segments: meter.u64_gauge("edamame.worker.cache.segments").build(),
            segments_downloaded: meter
                .u64_counter("edamame.worker.segments.downloaded")
                .build(),
            segments_served: meter.u64_counter("edamame.worker.segments.served").build(),
            errors: meter.u64_counter("edamame.worker.errors").build(),
        }
    }

    pub(crate) fn record_up(&self) {
        self.up.record(1, &[]);
    }

    pub(crate) fn record_controller_last_success_age(&self, age_s: f64) {
        self.controller_last_success_age.record(age_s, &[]);
    }

    pub(crate) fn record_cache(
        &self,
        channel_id: &str,
        quality: &str,
        head_sequence: u64,
        segments: u64,
    ) {
        let attributes = [
            KeyValue::new("channel_id", channel_id.to_owned()),
            KeyValue::new("quality", quality.to_owned()),
        ];
        self.cache_head_sequence.record(head_sequence, &attributes);
        self.cache_segments.record(segments, &attributes);
    }

    pub(crate) fn record_downloaded(&self, channel_id: &str, quality: &str) {
        self.segments_downloaded.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
            ],
        );
    }

    pub(crate) fn record_served(&self, channel_id: &str, quality: &str) {
        self.segments_served.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
            ],
        );
    }

    pub(crate) fn record_error(&self, operation: &'static str) {
        self.errors.add(1, &[KeyValue::new("operation", operation)]);
    }
}
