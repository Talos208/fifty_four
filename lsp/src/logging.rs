#[cfg(feature = "otel")]
use http::uri;
use std::env;
#[cfg(feature = "otel")]
use std::{eprintln, time::Duration};

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
use tracing::debug;
#[cfg(all(feature = "otel", debug_assertions))]
use tracing_subscriber::fmt::{self, format::FmtSpan};
#[cfg(feature = "otel")]
use {
    opentelemetry::{KeyValue, global, trace::TracerProvider as _},
    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge,
    opentelemetry_sdk::Resource,
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

/// OTel エクスポータ用レイヤ限定のフィルタ。`RUST_LOG` の値に関わらず常に TRACE まで通す
///  `reqwest`/`opentelemetry` を外すのは、「エクスポート → そのログもエクスポート」
/// というフィードバックループを断つため(stderr はどこにも再送されないのでこの心配は無い)。
#[cfg(feature = "otel")]
fn otel_filter() -> EnvFilter {
    suppress_transport_noise(EnvFilter::new("trace"))
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

/// 出力先の切り替え判定。`OTEL_EXPORTER_OTLP_ENDPOINT` が空でない値で設定されていれば
/// ネットワーク(OTel)行きとみなす(未設定・空文字は stderr 行き)。
#[cfg(feature = "otel")]
fn network_target() -> Option<String> {
    env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|v: &String| {
            if !v.is_empty() {
                // connectできるアドレスなら有効とみなす
                if let Ok(uri) = v.parse::<uri::Uri>() {
                    if let Some(authority) = uri.authority() {
                        if let Ok(addrs) =
                            std::net::ToSocketAddrs::to_socket_addrs(authority.as_str())
                        {
                            for addr in addrs {
                                let ip = std::net::SocketAddr::from(addr);
                                if let Ok(conn) = std::net::TcpStream::connect_timeout(
                                    &ip,
                                    Duration::from_secs(5),
                                ) {
                                    eprintln!("Success to connect");
                                    let _ = conn.shutdown(std::net::Shutdown::Both);
                                    return true;
                                }
                                eprintln!("can't connect to {:?}", ip);
                            }
                        } else {
                            eprintln!("failed to parse {:?}", authority);
                        }
                    } else {
                        eprintln!("no authority {:?}", uri);
                    }
                } else {
                    eprintln!("not uri {:?}", v);
                }
            }
            false
        })
}

#[cfg(feature = "otel")]
fn prepare_tracing(acp: bool) -> Logger {
    // 明示的に無効化されていれば、エクスポータもプロバイダも一切作らずに即終了する。
    if logging_disabled() {
        eprintln!("Logging disabled.");
        return Logger {
            tracer_provider: None,
            logger_provider: None,
            meter_provider: None,
        };
    }

    if network_target().filter(|t| !t.is_empty()).is_some() {
        return prepare_network_tracing(acp);
    }

    // 出力先が stderr のとき: ネットワークへは一切接続を試みない
    // (エクスポータ/プロバイダを作らない)。stderr へのミラーは開発時のみ
    // (配布バイナリでは省く。標準入出力を JSON-RPC チャネルとして使うため)。
    eprintln!("Logging to stderr.");
    #[cfg(debug_assertions)]
    prepare_stderr_tracing();

    Logger {
        tracer_provider: None,
        logger_provider: None,
        meter_provider: None,
    }
}

/// 出力先がネットワーク(OTel)のときの設定。stderr へは(エクスポータ構築失敗時を
/// 含めて)絶対に何も出さない。
#[cfg(feature = "otel")]
fn prepare_network_tracing(acp: bool) -> Logger {
    eprintln!("Logging to Otel.");
    let service_name = if acp {
        SERVICE_NAME_ACP
    } else {
        SERVICE_NAME_LSP
    };

    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    // エンドポイントは明示せず、opentelemetry-otlp 自身の解決順(シグナル別 env var →
    // 汎用 OTEL_EXPORTER_OTLP_ENDPOINT → 既定値 http://localhost:4317)に任せる。
    let tracer_provider = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .ok()
        .map(|span_exporter| {
            SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(span_exporter)
                .build()
        });

    let logger_provider = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
        .ok()
        .map(|log_exporter| {
            SdkLoggerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(log_exporter)
                .build()
        });

    let meter_provider = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .build()
        .ok()
        .map(|metric_exporter| {
            SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(metric_exporter)
                .build()
        });

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

    // stderr へは何も出さない。
    tracing_subscriber::registry()
        .with(otel_log_layer)
        .with(otel_trace_layer)
        .init();

    // panicをotel_log_layer経由でOtelへも送る
    // subscriberの init 後でないと届かないので、この位置に
    install_otel_panic_hook(logger_provider.clone());

    Logger {
        tracer_provider,
        logger_provider,
        meter_provider,
    }
}

/// panic発生時、その内容(メッセージ・発生位置)を `tracing::error!` として発行し、Otelへ送る。
///
/// panic直後にプロセスが異常終了するとバッチエクスポータの周期フラッシュを待てないため、
/// ログ発行後に明示的に `force_flush` する。
#[cfg(feature = "otel")]
fn install_otel_panic_hook(logger_provider: Option<SdkLoggerProvider>) {
    // 既存のhookも処理する
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);

        let location = info.location();
        let file = location.map(|l| l.file()).unwrap_or("unknown");
        let line = location.map(|l| l.line()).unwrap_or(0);

        // ファイル名、行番号の名前はOtelに合わせる
        tracing::error!(
            target: "panic",
            { log.file = file, log.line = line },
            "{}", info.payload_as_str().unwrap_or_default()
        );

        if let Some(provider) = &logger_provider {
            let _ = provider.force_flush();
        }
    }));
}

/// 秒精度(UTC)の時刻フォーマッタ。背景は `docs/observability.md` 参照
#[cfg(all(feature = "otel", debug_assertions))]
struct SecondsUtcTime;

#[cfg(all(feature = "otel", debug_assertions))]
impl fmt::time::FormatTime for SecondsUtcTime {
    fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
        static FORMAT: std::sync::LazyLock<
            Vec<time::format_description::BorrowedFormatItem<'static>>,
        > = std::sync::LazyLock::new(|| {
            time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
            )
            .expect("static time format description is valid")
        });

        let now = time::OffsetDateTime::now_utc();
        let s = now.format(&*FORMAT).map_err(|_| std::fmt::Error)?;
        write!(w, "{}", s)
    }
}

/// stderr 向けの素のログ行フォーマッタ。
#[cfg(all(feature = "otel", debug_assertions))]
struct PlainFormat;

#[cfg(all(feature = "otel", debug_assertions))]
impl<S, N> fmt::FormatEvent<S, N> for PlainFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &fmt::FmtContext<'_, S, N>,
        mut writer: fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use fmt::time::FormatTime;
        use tracing_log::NormalizeEvent;

        SecondsUtcTime.format_time(&mut writer)?;
        // `log::` 経由のイベントは file/line が失われるため normalized_metadata() で復元する。
        // target(モジュールパス)は意図的に出さない(`docs/observability.md` 参照)。
        let normalized_meta = event.normalized_metadata();
        let (level, file, line) = match &normalized_meta {
            Some(meta) => (meta.level(), meta.file(), meta.line()),
            None => {
                let meta = event.metadata();
                (meta.level(), meta.file(), meta.line())
            }
        };
        write!(writer, " {:>5} ", level)?;
        match (file, line) {
            (Some(file), Some(line)) => write!(writer, "{}:{}: ", file, line)?,
            (Some(file), None) => write!(writer, "{}: ", file)?,
            (None, _) => {}
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// 出力先が stderr のときの設定(開発ビルド限定)。span/trace 情報(span名の
/// プレフィックスや開始・終了イベント)は出さず、素のログ行だけにする。
/// ANSI エスケープシーケンスによる色装飾も無効化する。
#[cfg(all(feature = "otel", debug_assertions))]
fn prepare_stderr_tracing() {
    use tracing_subscriber::filter::{FilterExt, filter_fn};

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_span_events(FmtSpan::NONE)
                .event_format(PlainFormat)
                .with_filter(
                    // "message" フィールドの有無で #[instrument(ret)] の戻り値イベントを 除外する(span は残す)。`docs/observability.md` 参照。
                    suppress_transport_noise(EnvFilter::from_default_env()).and(filter_fn(
                        |meta| !meta.is_event() || meta.fields().field("message").is_some(),
                    )),
                ),
        )
        .init();

    debug!("Tracing initialized (stderr)");
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
