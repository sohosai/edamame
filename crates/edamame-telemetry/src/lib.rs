//! Edamame の controller / worker で共有する OpenTelemetry 計装
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` が設定されている場合だけ OTLP/gRPC で
//! メトリクスを送信する。未設定または `OTEL_SDK_DISABLED=true` の場合は
//! すべての計装が no-op になる。

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

/// メトリクスの既定の送信間隔
const DEFAULT_EXPORT_INTERVAL: Duration = Duration::from_secs(10);
/// 送信間隔の下限。0 や極端に短い値を弾く
const MIN_EXPORT_INTERVAL: Duration = Duration::from_millis(100);
/// OTLP の送信タイムアウト
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// 計装の初期化結果を保持するハンドル
///
/// アプリケーションの終了時に [`Telemetry::shutdown`] を呼ぶと、
/// 未送信のメトリクスを flush してから終了する。Drop でも shutdown する。
#[must_use = "保持しないとすぐに shutdown される"]
pub struct Telemetry {
    provider: Option<SdkMeterProvider>,
}

impl Telemetry {
    /// 計装が有効かどうか
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.provider.is_some()
    }

    /// メトリクスを flush して SDK を停止する
    ///
    /// 二回以上呼んでも安全。
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("[telemetry] failed to shutdown meter provider: {error}");
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Telemetry")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// OpenTelemetry のメトリクスを初期化する
///
/// `service_name` はリソースの `service.name`、`instance_id` は
/// `service.instance.id` に設定する。`OTEL_SERVICE_NAME` が設定されている
/// 場合はそちらが優先される。
///
/// 初期化に失敗してもアプリケーションは停止せず、no-op のハンドルを返す。
/// Tokio ランタイム内で呼ぶこと。
pub fn init(service_name: &str, instance_id: &str) -> Telemetry {
    let settings = Settings::from_env(service_name);
    if !settings.enabled {
        return Telemetry { provider: None };
    }

    match build_meter_provider(&settings, instance_id) {
        Ok(provider) => {
            global::set_meter_provider(provider.clone());
            Telemetry {
                provider: Some(provider),
            }
        }
        Err(error) => {
            eprintln!("[telemetry] metrics disabled: {error}");
            Telemetry { provider: None }
        }
    }
}

fn build_meter_provider(
    settings: &Settings,
    instance_id: &str,
) -> Result<SdkMeterProvider, String> {
    let endpoint = settings
        .endpoint
        .as_deref()
        .ok_or_else(|| "OTLP endpoint is not set".to_owned())?;

    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build OTLP metric exporter: {error}"))?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(settings.export_interval)
        .build();

    let resource = Resource::builder()
        .with_service_name(settings.service_name.clone())
        .with_attribute(KeyValue::new("service.instance.id", instance_id.to_owned()))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    Ok(SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader)
        .build())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    endpoint: Option<String>,
    service_name: String,
    export_interval: Duration,
}

impl Settings {
    fn from_env(service_name: &str) -> Self {
        Self::from_lookup(service_name, |key| std::env::var(key).ok())
    }

    fn from_lookup<F>(default_service_name: &str, lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let disabled = lookup("OTEL_SDK_DISABLED")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
        let endpoint = lookup("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .or_else(|| lookup("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(normalize_endpoint);
        let service_name = lookup("OTEL_SERVICE_NAME")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_service_name.to_owned());
        let export_interval = lookup("OTEL_METRIC_EXPORT_INTERVAL")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .filter(|interval| *interval >= MIN_EXPORT_INTERVAL)
            .unwrap_or(DEFAULT_EXPORT_INTERVAL);

        Self {
            enabled: !disabled && endpoint.is_some(),
            endpoint,
            service_name,
            export_interval,
        }
    }
}

fn normalize_endpoint(endpoint: String) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint
    } else {
        format!("http://{endpoint}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(vars: &[(&str, &str)]) -> Settings {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        Settings::from_lookup("edamame-controller", |key| {
            vars.get(key).map(|value| (*value).to_owned())
        })
    }

    #[test]
    fn disabled_when_endpoint_is_missing() {
        let settings = settings(&[]);

        assert!(!settings.enabled);
        assert_eq!(settings.endpoint, None);
        assert_eq!(settings.service_name, "edamame-controller");
        assert_eq!(settings.export_interval, DEFAULT_EXPORT_INTERVAL);
    }

    #[test]
    fn disabled_when_sdk_disabled() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://iromono:4317"),
            ("OTEL_SDK_DISABLED", "TRUE"),
        ]);

        assert!(!settings.enabled);
    }

    #[test]
    fn metrics_endpoint_overrides_generic_endpoint() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://generic:4317"),
            ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://metrics:4317"),
        ]);

        assert_eq!(settings.endpoint.as_deref(), Some("http://metrics:4317"));
    }

    #[test]
    fn service_name_env_overrides_default() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://iromono:4317"),
            ("OTEL_SERVICE_NAME", "custom-service"),
        ]);

        assert_eq!(settings.service_name, "custom-service");
    }

    #[test]
    fn endpoint_without_scheme_defaults_to_http() {
        let settings = settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "iromono:4317")]);

        assert_eq!(settings.endpoint.as_deref(), Some("http://iromono:4317"));
    }

    #[test]
    fn export_interval_can_be_overridden() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://iromono:4317"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "2500"),
        ]);

        assert_eq!(settings.export_interval, Duration::from_millis(2500));
    }

    #[test]
    fn invalid_export_interval_falls_back_to_default() {
        for value in ["", "abc", "0", "10"] {
            let settings = settings(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://iromono:4317"),
                ("OTEL_METRIC_EXPORT_INTERVAL", value),
            ]);

            assert_eq!(
                settings.export_interval, DEFAULT_EXPORT_INTERVAL,
                "value: {value:?}"
            );
        }
    }

    #[test]
    fn disabled_telemetry_is_safe_to_shutdown() {
        let mut telemetry = Telemetry { provider: None };

        assert!(!telemetry.is_enabled());
        telemetry.shutdown();
        telemetry.shutdown();
    }

    #[tokio::test]
    async fn builds_meter_provider_with_valid_settings() {
        let settings = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "1000"),
        ]);

        let provider = build_meter_provider(&settings, "test-instance")
            .expect("meter provider should be built");

        let _ = provider.shutdown();
    }
}
