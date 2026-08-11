use std::env;

#[cfg(feature = "otel")]
use opentelemetry_sdk::{
    logs::SdkLoggerProvider, metrics::SdkMeterProvider, trace::SdkTracerProvider,
};

#[allow(unused)]
pub struct Logger {
    #[cfg(feature = "otel")]
    tracer_provider: Option<SdkTracerProvider>,
    #[cfg(feature = "otel")]
    logger_provider: Option<SdkLoggerProvider>,
    #[cfg(feature = "otel")]
    meter_provider: Option<SdkMeterProvider>,
}

#[allow(unused)]
impl Logger {
    /// `acp` は ACP モード(`--acp`)かどうか。otel 有効時、`service.name` を
    /// LSP/ACP で分けるのに使う(otel 無効時は無視する)。
    pub fn new(acp: bool) -> Self {
        #[cfg(feature = "otel")]
        {
            prepare_tracing(acp)
        }
        #[cfg(not(feature = "otel"))]
        {
            prepare_env_logger();
            Logger {}
        }
    }
}

#[cfg(feature = "otel")]
impl Drop for Logger {
    fn drop(&mut self) {
        // shutdown() がバッチをフラッシュする。プロセス終了直前なのでエラーは握りつぶす。
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.logger_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

#[allow(unused)]
fn prepare_env_logger() {
    // In debug mode, default to info level if RUST_LOG is not set
    let mut lb = if env::var("RUST_LOG").is_err() {
        env_logger::Builder::from_default_env()
    } else {
        env_logger::Builder::from_env("RUST_LOG")
    };
    if env::var("FIFTY_FOUR_DEBUG").is_ok() {
        lb.filter_level(log::LevelFilter::Trace)
            .format_timestamp_millis()
            .format_module_path(true);
    }
    lb.init();
}

#[cfg(all(feature = "otel", debug_assertions))]
use tracing_subscriber::fmt;
#[cfg(feature = "otel")]
use {
    opentelemetry::{KeyValue, global, trace::TracerProvider as _},
    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge,
    opentelemetry_sdk::Resource,
    tracing::debug,
    tracing_subscriber::{
        EnvFilter, Layer, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
    },
};

/// LSP/ACP で service.name を分ける(ACPは別プロセスとして起動されるため、
/// コレクタ側でどちらの出力か区別できるようにする)。
#[cfg(feature = "otel")]
const SERVICE_NAME_LSP: &str = "fifty_four_lsp";
#[cfg(feature = "otel")]
const SERVICE_NAME_ACP: &str = "fifty_four_acp";

/// tonic/h2/hyper/tower が出す gRPC フレーム単位の低レベルログと、
/// OTel SDK 自身のバッチ処理スレッドが定期タイマーで吐く内部housekeepingログ
/// (`BatchLogProcessor.ExportingDueToTimer` 等、データの有無に関わらず一定間隔で発火する)
/// を抑制する。どちらもエクスポータの内部動作そのものであり、stderr・OTel 向けレイヤの
/// どちらで見てもノイズにしかならないため両方で外す。`opentelemetry_sdk` は完全な off では
/// なく warn までに留め、実際のエクスポート失敗など有用な情報は残す。
/// `EnvFilter` は `Clone` できないため、呼び出し側で都度これを起点に組み立てる。
#[cfg(feature = "otel")]
fn suppress_transport_noise(filter: EnvFilter) -> EnvFilter {
    filter
        .add_directive("hyper=off".parse().unwrap())
        .add_directive("h2=off".parse().unwrap())
        .add_directive("tonic=off".parse().unwrap())
        .add_directive("tower=off".parse().unwrap())
        .add_directive("opentelemetry_sdk=warn".parse().unwrap())
}

/// OTel エクスポータ用レイヤ限定のフィルタ。上記に加え `reqwest`/`opentelemetry` も外すのは、
/// 「エクスポート → そのログもエクスポート」というフィードバックループを断つため
/// (stderr はどこにも再送されないのでこの心配は無い)。
#[cfg(feature = "otel")]
fn otel_filter() -> EnvFilter {
    suppress_transport_noise(
        EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy(),
    )
    .add_directive("reqwest=off".parse().unwrap())
    .add_directive("opentelemetry=off".parse().unwrap())
}

/// `RUST_LOG=off`(または `fifty_four_lsp=off` 等)でログ出力が明示的に無効化されて
/// いるかどうか。`from_default_env()` は未設定時 ERROR 相当を返すため、「未設定」と
/// 「明示的な off」を正しく区別できる。
#[cfg(feature = "otel")]
fn logging_disabled() -> bool {
    EnvFilter::from_default_env().max_level_hint() == Some(LevelFilter::OFF)
}

#[cfg(feature = "otel")]
fn prepare_tracing(acp: bool) -> Logger {
    // 明示的に無効化されていれば、エクスポータもプロバイダも一切作らずに即終了する。
    if logging_disabled() {
        return Logger {
            tracer_provider: None,
            logger_provider: None,
            meter_provider: None,
        };
    }

    let service_name = if acp { SERVICE_NAME_ACP } else { SERVICE_NAME_LSP };

    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    // エンドポイントは明示せず、opentelemetry-otlp 自身の解決順(シグナル別 env var →
    // 汎用 OTEL_EXPORTER_OTLP_ENDPOINT → 既定値 http://localhost:4317)に任せる。
    let tracer_provider = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
    {
        Ok(span_exporter) => Some(
            SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(span_exporter)
                .build(),
        ),
        Err(e) => {
            eprintln!("Failed to create span exporter: {e}");
            None
        }
    };

    let logger_provider = match opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
    {
        Ok(log_exporter) => Some(
            SdkLoggerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(log_exporter)
                .build(),
        ),
        Err(e) => {
            eprintln!("Failed to create log exporter: {e}");
            None
        }
    };

    let meter_provider = match opentelemetry_otlp::MetricExporter::builder().with_tonic().build()
    {
        Ok(metric_exporter) => Some(
            SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(metric_exporter)
                .build(),
        ),
        Err(e) => {
            eprintln!("Failed to create metric exporter: {e}");
            None
        }
    };

    if let Some(tracer_provider) = &tracer_provider {
        global::set_tracer_provider(tracer_provider.clone());
    }
    if let Some(meter_provider) = &meter_provider {
        global::set_meter_provider(meter_provider.clone());
    }

    let otel_log_layer = logger_provider
        .as_ref()
        .map(|p| OpenTelemetryTracingBridge::new(p).with_filter(otel_filter()));

    let otel_trace_layer = tracer_provider.as_ref().map(|p| {
        tracing_opentelemetry::layer()
            .with_tracer(p.tracer(service_name))
            .with_filter(otel_filter())
    });

    // 標準入出力を JSON-RPC チャネルとして使うため、可読ログは stderr 限定。
    // stderr へのミラーは開発時のみ(配布バイナリでは省く)。
    #[cfg(debug_assertions)]
    tracing_subscriber::registry()
        .with(otel_log_layer)
        .with(otel_trace_layer)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(suppress_transport_noise(EnvFilter::from_default_env())),
        )
        .init();
    #[cfg(not(debug_assertions))]
    tracing_subscriber::registry()
        .with(otel_log_layer)
        .with(otel_trace_layer)
        .init();

    debug!("Tracing initialized");

    Logger {
        tracer_provider,
        logger_provider,
        meter_provider,
    }
}

#[cfg(all(test, feature = "otel"))]
mod tests {
    use super::*;

    use crate::RUST_LOG_TEST_LOCK as ENV_LOCK;

    #[test]
    fn test_logging_disabled_when_rust_log_is_off() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("RUST_LOG", "off") };

        assert!(logging_disabled());

        unsafe { std::env::remove_var("RUST_LOG") };
    }

    #[test]
    fn test_logging_disabled_when_rust_log_scopes_off_to_this_crate() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("RUST_LOG", "fifty_four_lsp=off") };

        assert!(logging_disabled());

        unsafe { std::env::remove_var("RUST_LOG") };
    }

    #[test]
    fn test_logging_not_disabled_when_rust_log_is_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("RUST_LOG") };

        assert!(!logging_disabled());
    }

    #[test]
    fn test_logging_not_disabled_when_rust_log_has_a_real_level() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("RUST_LOG", "debug") };

        assert!(!logging_disabled());

        unsafe { std::env::remove_var("RUST_LOG") };
    }
}
