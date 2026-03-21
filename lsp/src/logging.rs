use std::env;

#[allow(unused)]
pub struct Logger {
    pub tracer_provider: Option<SdkTracerProvider>,
    pub tracer: Option<BoxedTracer>,
    pub logger_provider: Option<SdkLoggerProvider>,
}

#[allow(unused)]
impl Logger {
    pub fn new() -> Self {
        if true {
            prepare_tracing()
        } else {
            prepare_env_logger();
            Logger {
                tracer_provider: None,
                tracer: None,
                logger_provider: None,
            }
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

use opentelemetry::KeyValue;
use opentelemetry::global::{self, BoxedTracer};
// use opentelemetry::logs::{LoggerProvider, NoopLoggerProvider};
// use opentelemetry::trace::TracerProvider;
// use opentelemetry_appender_log::OpenTelemetryLogBridge;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
// use opentelemetry_otlp;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
#[allow(unused_imports)]
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
// use opentelemetry_stdout::{LogExporter, SpanExporter};
use tracing::debug;
// use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

impl Drop for Logger {
    fn drop(&mut self) {
        if let Some(tracer) = self.tracer.take() {
            drop(tracer);
        }
        if let Some(provider) = self.tracer_provider.take() {
            provider.shutdown().unwrap();
        }
        if let Some(provider) = self.logger_provider.take() {
            provider.shutdown().unwrap();
        }
    }
}

#[allow(unused)]
fn prepare_tracing() -> Logger {
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint("http://localhost:4317")
        .build()
        .expect("Failed to create spen exporter");

    // let span_exporter_stderr = SpanExporter::builder().with_writer(std::io::stderr).build();

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(Resource::builder().with_service_name("g_sync_now").build())
        .with_batch_exporter(span_exporter)
        .build();

    global::set_tracer_provider(tracer_provider.clone());

    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint("http://localhost:4317")
        .build()
        .expect("Failed to create log exporter");

    // let log_exporter_stderr = opentelemetry_stdout::LogExporter::builder()
    //     .with_writer(std::io::stderr)
    //     .build();

    let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(
            Resource::builder()
                .with_service_name("g_sync_now")
                .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
                .build(),
        )
        .build();

    let otel_trace_layer = OpenTelemetryTracingBridge::new(&logger_provider);
    // let otel_log_layer = OpenTelemetryLogBridge::new(&logger_provider);

    let otel_filter = EnvFilter::from_default_env()
        .add_directive("hyper=off".parse().unwrap())
        .add_directive("tonic=off".parse().unwrap())
        .add_directive("h2=off".parse().unwrap())
        .add_directive("reqwest=off".parse().unwrap());

    let ls = logging_subscriber::LoggingSubscriberBuilder::default()
        .with_file(true)
        .with_line_number(true)
        .build();

    tracing_subscriber::registry()
        .with(otel_trace_layer)
        .with(ls)
        .with(otel_filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    debug!("Tracing initialized");

    Logger {
        tracer: None,
        tracer_provider: None,
        logger_provider: Some(logger_provider),
    }
}
