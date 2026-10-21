//! controller の OpenTelemetry メトリクス

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Gauge};

/// controller が記録するメトリクスのハンドル
pub(crate) struct ControllerMetrics {
    up: Gauge<i64>,
    worker_last_sync_age: Gauge<f64>,
    worker_weight: Gauge<i64>,
    channel_last_ingest_age: Gauge<f64>,
    channel_target_duration: Gauge<f64>,
    channel_published_lag: Gauge<f64>,
    segments_ingested: Counter<u64>,
    segments_published: Counter<u64>,
    errors: Counter<u64>,
}

impl ControllerMetrics {
    pub(crate) fn new() -> Self {
        let meter = global::meter("edamame-controller");
        Self {
            up: meter.i64_gauge("edamame.controller.up").build(),
            worker_last_sync_age: meter
                .f64_gauge("edamame.controller.worker.last_sync_age")
                .with_unit("s")
                .build(),
            worker_weight: meter.i64_gauge("edamame.controller.worker.weight").build(),
            channel_last_ingest_age: meter
                .f64_gauge("edamame.controller.channel.last_ingest_age")
                .with_unit("s")
                .build(),
            channel_target_duration: meter
                .f64_gauge("edamame.controller.channel.target_duration")
                .with_unit("s")
                .build(),
            channel_published_lag: meter
                .f64_gauge("edamame.controller.channel.published_lag")
                .with_unit("s")
                .build(),
            segments_ingested: meter
                .u64_counter("edamame.controller.segments.ingested")
                .build(),
            segments_published: meter
                .u64_counter("edamame.controller.segments.published")
                .build(),
            errors: meter.u64_counter("edamame.controller.errors").build(),
        }
    }

    pub(crate) fn record_up(&self) {
        self.up.record(1, &[]);
    }

    pub(crate) fn record_worker(&self, worker_id: &str, last_sync_age_s: f64, weight: i64) {
        let attributes = [KeyValue::new("worker_id", worker_id.to_owned())];
        self.worker_last_sync_age
            .record(last_sync_age_s, &attributes);
        self.worker_weight.record(weight, &attributes);
    }

    pub(crate) fn record_channel(
        &self,
        channel_id: &str,
        last_ingest_age_s: f64,
        target_duration_s: f64,
        published_lag_s: f64,
    ) {
        let attributes = [KeyValue::new("channel_id", channel_id.to_owned())];
        self.channel_last_ingest_age
            .record(last_ingest_age_s, &attributes);
        self.channel_target_duration
            .record(target_duration_s, &attributes);
        self.channel_published_lag
            .record(published_lag_s, &attributes);
    }

    pub(crate) fn record_ingested(&self, channel_id: &str, quality: &str) {
        self.segments_ingested.add(
            1,
            &[
                KeyValue::new("channel_id", channel_id.to_owned()),
                KeyValue::new("quality", quality.to_owned()),
            ],
        );
    }

    pub(crate) fn record_published(&self, channel_id: &str, quality: &str, segments: u64) {
        if segments == 0 {
            return;
        }
        self.segments_published.add(
            segments,
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
