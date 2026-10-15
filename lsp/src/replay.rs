//! `--replay`: 記録されたプロンプトを任意の LLM に再実行する HTTP サーバー(feature `replay`)。
//!
//! 仕様: docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::llm::{Content, LlmClientBuilder, LlmError, LlmInterface, Provider};

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(crate) const MAX_TIMEOUT_SECS: u64 = 600;
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_CHARS: usize = 1_000_000;
pub(crate) const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
/// k8s の readiness / liveness 用。トレースの対象外。
pub(crate) const HEALTH_PATH: &str = "/healthz";

#[derive(Debug, Deserialize)]
pub(crate) struct ReplayRequest {
    /// `provider/model` または `lmstudio@host:port/model`(`Provider::from_name` の書式)
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// システムプロンプト。省略時は LSP の執筆支援と同じ `system.md`。空文字ならシステムプロンプトなし。
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct ReplayResponse {
    pub response: String,
    pub model: String,
    pub elapsed_ms: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    BadRequest,
    Auth,
    Timeout,
    Llm,
    Internal,
}

impl ErrorKind {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Auth | ErrorKind::Llm => StatusCode::BAD_GATEWAY,
            ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ReplayError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ReplayError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }
}

impl IntoResponse for ReplayError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "kind": self.kind, "message": self.message }
        });
        (self.kind.status(), Json(body)).into_response()
    }
}

impl ReplayRequest {
    /// 入力を検証し、有効なタイムアウトを返す。
    #[cfg_attr(
        feature = "otel",
        tracing::instrument(
            skip(self),
            fields(model = %self.model, timeout_secs = ?self.timeout_secs),
            ret,
            err(Debug)
        )
    )]
    pub(crate) fn validate(&self) -> Result<Duration, ReplayError> {
        if self.prompt.trim().is_empty() {
            return Err(ReplayError::bad_request("prompt が空です"));
        }
        let model = self.model.trim();
        if model.is_empty() {
            return Err(ReplayError::bad_request("model が空です"));
        }
        // `google/` や `lmstudio@host:1234/` のようにモデル部が空のものを弾く
        if model.rsplit('/').next().is_none_or(|m| m.trim().is_empty()) {
            return Err(ReplayError::bad_request(format!(
                "model のモデル名の部分が空です: {model}"
            )));
        }
        let provider = Provider::from_name(model).map_err(ReplayError::bad_request)?;
        // account_id を渡す経路が無く、Cloudflare は組み立てる URL が不正になる。
        // 文字列の接頭辞ではなく解決後のプロバイダで判定する(素の "cloudflare" も該当する)。
        if matches!(provider, Provider::Cloudflare(_)) {
            return Err(ReplayError::bad_request(
                "cloudflare は replay では未対応です(account_id を指定できません)",
            ));
        }

        let secs = match self.timeout_secs {
            None => DEFAULT_TIMEOUT_SECS,
            Some(0) => {
                return Err(ReplayError::bad_request(
                    "timeout_secs は 1 以上にしてください",
                ));
            }
            Some(n) => n.min(MAX_TIMEOUT_SECS),
        };
        Ok(Duration::from_secs(secs))
    }
}

pub(crate) fn classify_llm_error(e: &LlmError) -> ReplayError {
    match e {
        LlmError::GenericError { message } => {
            let lower = message.to_lowercase();
            // 認証エラーは `GenericError` に載ってくるため、メッセージ文字列で判定する
            const AUTH_HINTS: [&str; 7] = [
                "401",
                "403",
                "unauthorized",
                "forbidden",
                "api key",
                "api_key",
                "authentication",
            ];
            if AUTH_HINTS.iter().any(|h| lower.contains(h)) {
                ReplayError::new(ErrorKind::Auth, message.clone())
            } else {
                ReplayError::new(ErrorKind::Llm, message.clone())
            }
        }
        LlmError::NotInitialized => ReplayError::new(ErrorKind::Internal, e.to_string()),
        other => ReplayError::new(ErrorKind::Llm, other.to_string()),
    }
}

#[cfg_attr(
    feature = "otel",
    tracing::instrument(skip_all, fields(max, truncated = tracing::field::Empty))
)]
pub(crate) fn truncate_chars(text: String, max: usize) -> (String, bool) {
    let (text, truncated) = match text.char_indices().nth(max) {
        Some((byte_idx, _)) => (text[..byte_idx].to_string(), true),
        None => (text, false),
    };
    record("truncated", &truncated);
    (text, truncated)
}

/// 現在のスパンへ値を記録する
/// `fields(.. = tracing::field::Empty)` で宣言したものだけが対象
#[cfg(feature = "otel")]
fn record<V: tracing::Value + ?Sized>(field: &str, value: &V) {
    tracing::Span::current().record(field, value);
}

#[cfg(not(feature = "otel"))]
fn record<V: ?Sized>(_field: &str, _value: &V) {}

/// LLM クライアントの生成を差し替えられるようにするための境界(テストはネットワークに出ない)。
#[async_trait]
pub(crate) trait ClientFactory: Send + Sync {
    async fn create(
        &self,
        model: &str,
        system: Option<String>,
    ) -> Result<Box<dyn LlmInterface>, ReplayError>;
}

pub(crate) struct RealFactory;

#[async_trait]
impl ClientFactory for RealFactory {
    #[cfg_attr(
        feature = "otel",
        tracing::instrument(
            skip_all,
            fields(model = model, has_system = system.is_some()),
            err(Debug)
        )
    )]
    async fn create(
        &self,
        model: &str,
        system: Option<String>,
    ) -> Result<Box<dyn LlmInterface>, ReplayError> {
        let sys_prompt = match system {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
            None => crate::assets::load("system.md"),
        };
        // `validate()` で `Provider::from_name` が通っているので、ここで panic しない。
        Ok(LlmClientBuilder::from_name(model)
            .sys_prompt(sys_prompt)
            .build_async()
            .await)
    }
}

pub(crate) struct AppState {
    pub factory: Arc<dyn ClientFactory>,
    pub permits: Arc<Semaphore>,
}

impl AppState {
    pub(crate) fn new(factory: Arc<dyn ClientFactory>, max_concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            factory,
            permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
        })
    }
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    let router = Router::new()
        .route("/v1/replay", post(replay_handler))
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state);
    #[cfg(feature = "otel")]
    let router = router.layer(axum::middleware::from_fn(trace_context));
    router
}

/// W3C Trace Context(`traceparent` ヘッダ)の取り込み
/// `traceparent` だけは、正規化した値(`normalize_traceparent`)があればそれを返す
#[cfg(feature = "otel")]
struct HeaderMapExtractor<'a> {
    headers: &'a axum::http::HeaderMap,
    traceparent: Option<&'a str>,
}

#[cfg(feature = "otel")]
impl opentelemetry::propagation::Extractor for HeaderMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        if key == "traceparent"
            && let Some(normalized) = self.traceparent
        {
            return Some(normalized);
        }
        self.headers.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.keys().map(|k| k.as_str()).collect()
    }
}

/// グローバルの propagator(headers[`run`])が設定済みであること
#[cfg(feature = "otel")]
pub(crate) fn extract_parent_context(headers: &axum::http::HeaderMap) -> opentelemetry::Context {
    let normalized = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_traceparent);
    let extractor = HeaderMapExtractor {
        headers,
        traceparent: normalized.as_deref(),
    };
    opentelemetry::global::get_text_map_propagator(|p| p.extract(&extractor))
}

#[cfg(feature = "otel")]
fn normalize_traceparent(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.trim().split('-').collect();
    let [version, trace_id, span_id, flags] = parts.as_slice() else {
        return None;
    };
    if *version != "00" || flags.len() != 2 {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;
    // trace-flagsはsampled ビットだけ残す
    // opentelemetry_sdk 0.31 の propagator は version 00かつflags > 2 を不正として捨ててしまう
    Some(format!("00-{trace_id}-{span_id}-{:02x}", flags & 0x01))
}

#[cfg(feature = "otel")]
async fn trace_context(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    use tracing::Instrument;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    // k8s のプローブは頻繁に来てトレースを埋めるので、スパンを作らない
    if req.uri().path() == HEALTH_PATH {
        return next.run(req).await;
    }

    let span = tracing::info_span!(
        "replay.http",
        otel.kind = "server",
        http.request.method = %req.method(),
        url.path = %req.uri().path(),
    );

    // shinasadame が送ってきた `traceparent` の子にする
    // 親が無い/不正でも失敗でなく新しいトレースに
    let _ = span.set_parent(extract_parent_context(req.headers()));
    next.run(req).instrument(span).await
}

#[cfg_attr(
    feature = "otel",
    tracing::instrument(
        skip_all,
        fields(
            model = tracing::field::Empty,
            prompt = tracing::field::Empty,
            response = tracing::field::Empty,
            timeout_secs = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            truncated = tracing::field::Empty,
            response_chars = tracing::field::Empty,
        ),
        err(Debug)
    )
)]
async fn replay_handler(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ReplayRequest>, JsonRejection>,
) -> Result<Json<ReplayResponse>, ReplayError> {
    // 不正な JSON・Content-Type 欠落・本文が上限超過は、すべて 400 `bad_request` に揃える。
    let Json(req) = body.map_err(|e| {
        ReplayError::bad_request(format!("リクエストを解釈できません: {}", e.body_text()))
    })?;

    record("model", req.model.trim());
    record("prompt", &req.prompt);
    let timeout = req.validate()?;
    record("timeout_secs", &timeout.as_secs());

    // 同時実行の上限。超えた分は拒否せずタイムアウトまで待機
    let _permit = state
        .permits
        .acquire()
        .await
        .map_err(|_| ReplayError::new(ErrorKind::Internal, "同時実行の制御に失敗しました"))?;

    let started = Instant::now();
    let mut client = state
        .factory
        .create(req.model.trim(), req.system.clone())
        .await?;
    client.add(Content::Text(req.prompt.clone()));

    // オプションが空なので `use_llm_with_option` は使わず、新規クライアントを直接呼ぶ。
    let text = match tokio::time::timeout(timeout, client.chat()).await {
        Err(_) => {
            return Err(ReplayError::new(
                ErrorKind::Timeout,
                format!("{} 秒以内に応答がありませんでした", timeout.as_secs()),
            ));
        }
        Ok(Err(e)) => {
            let mut err = classify_llm_error(&e);
            if err.kind == ErrorKind::Auth {
                // どの provider の認証かを示す(キー値は含めない)。
                let label = Provider::from_name(req.model.trim()).map_or("unknown", |p| p.label());
                err.message = format!("認証に失敗しました ({label}): {}", err.message);
            }
            return Err(err);
        }
        Ok(Ok(text)) => text,
    };

    let (response, truncated) = truncate_chars(text, MAX_RESPONSE_CHARS);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    record("elapsed_ms", &elapsed_ms);
    record("response", &response);
    record("truncated", &truncated);
    record("response_chars", &response.chars().count());
    Ok(Json(ReplayResponse {
        response,
        model: req.model.trim().to_string(),
        elapsed_ms,
        truncated,
    }))
}

#[cfg_attr(feature = "otel", tracing::instrument(ret))]
pub(crate) fn parse_listen(
    args: &[String],
    env_listen: Option<String>,
) -> Result<SocketAddr, String> {
    // 優先順位: `--listen` > 環境変数 `REPLAY_LISTEN` > 既定
    let from_flag = match args.iter().position(|a| a == "--listen") {
        Some(i) => Some(
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| "--listen にアドレスがありません".to_string())?,
        ),
        None => None,
    };
    let raw = from_flag
        .or(env_listen)
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    raw.parse::<SocketAddr>()
        .map_err(|e| format!("待ち受けアドレスが不正です ({raw}): {e}"))
}

#[cfg_attr(feature = "otel", tracing::instrument(ret))]
pub(crate) fn max_concurrency_from(env_value: Option<String>) -> usize {
    env_value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
}

pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Ctrl+C または SIGTERM(k8s の Pod 停止)を待つ。
#[cfg_attr(feature = "otel", tracing::instrument)]
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => log::info!("replay: Ctrl+C を受けたので停止する"),
        _ = terminate => log::info!("replay: SIGTERM を受けたので停止する"),
    }
}

/// `--replay` のエントリポイント。標準出力は使わない(ログは stderr / OTLP)。
pub(crate) fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = match parse_listen(&args, std::env::var("REPLAY_LISTEN").ok()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fifty_four_lsp --replay: {e}");
            std::process::exit(2);
        }
    };
    let max = max_concurrency_from(std::env::var("REPLAY_MAX_CONCURRENCY").ok());
    run(addr, max);
}

#[tokio::main]
async fn run(addr: SocketAddr, max_concurrency: usize) {
    // LSP/ACP と同じロガー。`service.name` は `fifty_four_replay` になる(LogMode::Replay)。
    let _log = crate::logging::Logger::new(crate::logging::LogMode::Replay);

    // shinasadame(Python の OTel)が送る W3C `traceparent` を解釈する
    #[cfg(feature = "otel")]
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fifty_four_lsp --replay: {addr} で待ち受けできません: {e}");
            drop(_log);
            std::process::exit(1);
        }
    };
    log::info!("replay server listening on {addr} (max_concurrency={max_concurrency})");

    let state = AppState::new(Arc::new(RealFactory), max_concurrency);
    if let Err(e) = serve(listener, state, shutdown_signal()).await {
        eprintln!("fifty_four_lsp --replay: {e}");
        drop(_log);
        std::process::exit(1);
    }
    log::info!("replay server stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use genai::chat::{ChatResponseFormat, ReasoningEffort, ServiceTier, Verbosity};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::llm::{Content, LlmInterface, LlmTool, ModelCapability};

    /// テスト専用の最小 LlmInterface。`Behavior` どおりに `chat()` する。
    #[derive(Debug, Clone)]
    enum Behavior {
        Reply(String),
        Fail(String),
        Hang,
        /// `chat()` に入ったら in-flight を数え、テストが `open` に許可を足すまで待つ。
        Gate(std::sync::Arc<Gate>),
    }

    #[derive(Debug)]
    struct Gate {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        open: tokio::sync::Semaphore,
    }

    impl Gate {
        fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                open: tokio::sync::Semaphore::new(0),
            })
        }
    }

    #[derive(Debug)]
    struct StubLlm {
        behavior: Behavior,
        added: Vec<String>,
        seen: std::sync::Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl LlmInterface for StubLlm {
        async fn chat(&mut self) -> Result<String, LlmError> {
            *self.seen.lock().unwrap() = self.added.clone();
            match &self.behavior {
                Behavior::Reply(s) => Ok(s.clone()),
                Behavior::Fail(m) => Err(LlmError::GenericError { message: m.clone() }),
                Behavior::Gate(g) => {
                    let now = g.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    g.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    g.open.acquire().await.unwrap().forget();
                    g.in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok("ok".to_string())
                }
                Behavior::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
        async fn with_model(&mut self, _model: &str) -> Result<String, LlmError> {
            Ok(String::new())
        }
        fn add(&mut self, prompt: Content) {
            self.added.push(prompt.to_string());
        }
        fn build_content(&self) -> String {
            String::new()
        }
        fn get_model(&self) -> &str {
            "stub-model"
        }
        fn clear(&mut self) {}
        fn cache(&mut self, _prompt: Content) -> Result<String, LlmError> {
            Ok(String::new())
        }
        fn fetch(&self, _hash: &str) -> Option<&Content> {
            None
        }
        fn fetch_all(&self) -> Vec<Content> {
            Vec::new()
        }
        fn remove(&mut self, _hash: String) {}
        fn capabilities(&self) -> ModelCapability {
            ModelCapability::empty()
        }
        fn max_tokens(&mut self, _n: u32) {}
        fn temperature(&mut self, _v: f64) {}
        fn top_p(&mut self, _v: f64) {}
        fn stop_sequences(&mut self, _seqs: Vec<String>) {}
        fn seed(&mut self, _v: u64) {}
        fn reasoning_effort(&mut self, _effort: ReasoningEffort) {}
        fn reasoning_level(&mut self, _level: f64) {}
        fn response_format(&mut self, _fmt: ChatResponseFormat) {}
        fn service_tier(&mut self, _tier: ServiceTier) {}
        fn verbosity(&mut self, _v: Verbosity) {}
        fn add_tool(&mut self, _tool: Box<dyn LlmTool>) {}
        async fn respond_tool(
            &self,
            _tools: &[genai::chat::ToolCall],
        ) -> Result<genai::chat::ChatRequest, LlmError> {
            Err(LlmError::NotImplemented)
        }
    }

    /// `ClientFactory` のテスト用実装。作成回数と、`chat()` 時にクライアントへ渡されていたプロンプトを記録する。
    struct StubFactory {
        behavior: Behavior,
        created: AtomicUsize,
        last_system: StdMutex<Option<Option<String>>>,
        seen_prompts: std::sync::Arc<StdMutex<Vec<String>>>,
    }

    impl StubFactory {
        fn new(behavior: Behavior) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                behavior,
                created: AtomicUsize::new(0),
                last_system: StdMutex::new(None),
                seen_prompts: std::sync::Arc::new(StdMutex::new(Vec::new())),
            })
        }
    }

    #[async_trait]
    impl ClientFactory for StubFactory {
        async fn create(
            &self,
            _model: &str,
            system: Option<String>,
        ) -> Result<Box<dyn LlmInterface>, ReplayError> {
            self.created.fetch_add(1, Ordering::SeqCst);
            *self.last_system.lock().unwrap() = Some(system);
            Ok(Box::new(StubLlm {
                behavior: self.behavior.clone(),
                added: Vec::new(),
                seen: self.seen_prompts.clone(),
            }))
        }
    }

    fn app(factory: std::sync::Arc<StubFactory>, max_concurrency: usize) -> axum::Router {
        router(AppState::new(factory, max_concurrency))
    }

    async fn post_json(app: axum::Router, body: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/replay")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.oneshot(request).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    const OK_BODY: &str = r#"{"model":"google/gemini-3.1-pro","prompt":"続きを書いて"}"#;

    #[tokio::test]
    async fn test_replay_returns_response_model_and_elapsed() {
        let f = StubFactory::new(Behavior::Reply("返答です".to_string()));
        let (status, json) = post_json(app(f.clone(), 4), OK_BODY).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["response"], "返答です");
        assert_eq!(json["model"], "google/gemini-3.1-pro");
        assert_eq!(json["truncated"], false);
        assert!(json["elapsed_ms"].is_u64());
        // 記録されたプロンプトがそのままユーザープロンプトとして渡る
        assert_eq!(
            *f.seen_prompts.lock().unwrap(),
            vec!["続きを書いて".to_string()]
        );
    }

    #[tokio::test]
    async fn test_replay_passes_system_override_and_default_to_factory() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        post_json(app(f.clone(), 4), OK_BODY).await;
        assert_eq!(
            *f.last_system.lock().unwrap(),
            Some(None),
            "省略は None(=system.md)"
        );

        let body = r#"{"model":"google/gemini-3.1-pro","prompt":"x","system":""}"#;
        post_json(app(f.clone(), 4), body).await;
        assert_eq!(*f.last_system.lock().unwrap(), Some(Some(String::new())));
    }

    #[tokio::test]
    async fn test_replay_bad_json_is_bad_request_not_422() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let (status, json) = post_json(app(f.clone(), 4), "これはJSONではない").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
        assert_eq!(
            f.created.load(Ordering::SeqCst),
            0,
            "検証で弾けばクライアントを作らない"
        );
    }

    #[tokio::test]
    async fn test_replay_missing_field_is_bad_request() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let (status, json) = post_json(app(f, 4), r#"{"model":"google/gemini-3.1-pro"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
    }

    #[tokio::test]
    async fn test_replay_validation_errors_do_not_create_a_client() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        for body in [
            r#"{"model":"google/gemini-3.1-pro","prompt":"   "}"#,
            r#"{"model":"google/","prompt":"x"}"#,
            r#"{"model":"nosuch/m","prompt":"x"}"#,
        ] {
            let (status, json) = post_json(app(f.clone(), 4), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
            assert_eq!(json["error"]["kind"], "bad_request");
        }
        assert_eq!(f.created.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_replay_auth_error_is_502_auth() {
        let f = StubFactory::new(Behavior::Fail("HTTP 401 Unauthorized".to_string()));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["error"]["kind"], "auth");
        let msg = json["error"]["message"].as_str().unwrap();
        assert!(msg.contains("google"), "provider missing: {msg}");
        assert!(msg.contains("401"), "upstream text missing: {msg}");
    }

    #[tokio::test]
    async fn test_replay_llm_error_is_502_llm() {
        let f = StubFactory::new(Behavior::Fail("model overloaded".to_string()));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["error"]["kind"], "llm");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("model overloaded")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_replay_timeout_is_504_and_server_keeps_working() {
        let hang = StubFactory::new(Behavior::Hang);
        let body = r#"{"model":"google/gemini-3.1-pro","prompt":"x","timeout_secs":1}"#;
        let (status, json) = post_json(app(hang, 4), body).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json["error"]["kind"], "timeout");
    }

    #[tokio::test]
    async fn test_replay_truncates_long_multibyte_response() {
        let long = "あ😀".repeat(MAX_RESPONSE_CHARS); // 2 * MAX 文字
        let f = StubFactory::new(Behavior::Reply(long));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["truncated"], true);
        assert_eq!(
            json["response"].as_str().unwrap().chars().count(),
            MAX_RESPONSE_CHARS
        );
    }

    #[tokio::test]
    async fn test_replay_rejects_body_over_limit_as_bad_request() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let huge = "あ".repeat(MAX_BODY_BYTES); // バイト数では上限の 3 倍
        let body =
            serde_json::json!({"model": "google/gemini-3.1-pro", "prompt": huge}).to_string();
        let (status, json) = post_json(app(f.clone(), 4), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
        assert_eq!(f.created.load(Ordering::SeqCst), 0);
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn test_replay_limit_one_makes_the_second_request_wait_not_fail() {
        let gate = Gate::new();
        let f = StubFactory::new(Behavior::Gate(gate.clone()));
        let a = app(f.clone(), 1);
        let t1 = tokio::spawn(post_json(a.clone(), OK_BODY));
        let t2 = tokio::spawn(post_json(a, OK_BODY));
        settle().await;
        // 1 件だけが LLM 呼び出し中で、2 件目はクライアント生成前に許可待ち。
        assert_eq!(gate.in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(f.created.load(Ordering::SeqCst), 1);
        assert!(!t1.is_finished() && !t2.is_finished());

        gate.open.add_permits(2);
        let (r1, r2) = (t1.await.unwrap(), t2.await.unwrap());
        assert_eq!(r1.0, StatusCode::OK);
        assert_eq!(r2.0, StatusCode::OK);
        assert_eq!(f.created.load(Ordering::SeqCst), 2);
        assert_eq!(gate.max_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_replay_limit_two_runs_two_requests_concurrently() {
        let gate = Gate::new();
        let f = StubFactory::new(Behavior::Gate(gate.clone()));
        let a = app(f, 2);
        let t1 = tokio::spawn(post_json(a.clone(), OK_BODY));
        let t2 = tokio::spawn(post_json(a, OK_BODY));
        settle().await;
        assert_eq!(gate.in_flight.load(Ordering::SeqCst), 2);

        gate.open.add_permits(2);
        assert_eq!(t1.await.unwrap().0, StatusCode::OK);
        assert_eq!(t2.await.unwrap().0, StatusCode::OK);
        assert_eq!(gate.max_in_flight.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_healthz_returns_ok() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let request = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let res = app(f, 4).oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"ok");
    }

    fn req(model: &str, prompt: &str) -> ReplayRequest {
        ReplayRequest {
            model: model.to_string(),
            prompt: prompt.to_string(),
            timeout_secs: None,
            system: None,
        }
    }

    #[test]
    fn test_validate_accepts_known_provider_and_returns_default_timeout() {
        let t = req("google/gemini-3.1-pro", "こんにちは")
            .validate()
            .unwrap();
        assert_eq!(t, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn test_validate_clamps_timeout_to_max() {
        let mut r = req("google/gemini-3.1-pro", "x");
        r.timeout_secs = Some(10_000);
        assert_eq!(r.validate().unwrap(), Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn test_validate_rejects_zero_timeout() {
        let mut r = req("google/gemini-3.1-pro", "x");
        r.timeout_secs = Some(0);
        assert_eq!(r.validate().unwrap_err().kind, ErrorKind::BadRequest);
    }

    #[test]
    fn test_validate_rejects_empty_and_whitespace_prompt() {
        for p in ["", "   ", "\n\t "] {
            let e = req("google/gemini-3.1-pro", p).validate().unwrap_err();
            assert_eq!(e.kind, ErrorKind::BadRequest, "prompt={p:?}");
        }
    }

    #[test]
    fn test_validate_rejects_empty_model_and_empty_model_part() {
        for m in ["", "  ", "google/", "lmstudio@localhost:1234/"] {
            let e = req(m, "x").validate().unwrap_err();
            assert_eq!(e.kind, ErrorKind::BadRequest, "model={m:?}");
        }
    }

    #[test]
    fn test_validate_rejects_unknown_provider_without_panicking() {
        let e = req("nosuch/model", "x").validate().unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("nosuch/model"), "{}", e.message);
    }

    #[test]
    fn test_validate_rejects_cloudflare_because_account_id_cannot_be_given() {
        for m in ["cloudflare", "cloudflare/@cf/meta/llama-3.1-8b-instruct"] {
            let e = req(m, "x").validate().unwrap_err();
            assert_eq!(e.kind, ErrorKind::BadRequest, "model={m:?}");
            assert!(e.message.contains("cloudflare"), "{}", e.message);
        }
    }

    #[test]
    fn test_validate_accepts_padded_model_because_it_trims() {
        // validate() は model を trim して判定する。呼び出し側も trim 済みの値を使うこと。
        assert!(req("  google/gemini-3.1-pro  ", "x").validate().is_ok());
    }

    #[test]
    fn test_classify_auth_errors_by_message() {
        for msg in [
            "HTTP 401 Unauthorized",
            "status 403 Forbidden",
            "API key not valid",
            "env var GEMINI_API_KEY not found (api_key)",
            "authentication failed",
        ] {
            let e = classify_llm_error(&LlmError::GenericError {
                message: msg.to_string(),
            });
            assert_eq!(e.kind, ErrorKind::Auth, "msg={msg}");
            assert!(e.message.contains(msg), "原文を失わない: {}", e.message);
        }
    }

    #[test]
    fn test_classify_other_generic_error_is_llm() {
        let e = classify_llm_error(&LlmError::GenericError {
            message: "model overloaded".to_string(),
        });
        assert_eq!(e.kind, ErrorKind::Llm);
    }

    #[test]
    fn test_classify_busy_is_llm_and_not_initialized_is_internal() {
        assert_eq!(
            classify_llm_error(&LlmError::LlmBusy { retry_after: 3 }).kind,
            ErrorKind::Llm
        );
        assert_eq!(
            classify_llm_error(&LlmError::NotInitialized).kind,
            ErrorKind::Internal
        );
    }

    #[test]
    fn test_error_kind_maps_to_http_status() {
        assert_eq!(ErrorKind::BadRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorKind::Auth.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(ErrorKind::Timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(ErrorKind::Llm.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            ErrorKind::Internal.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_truncate_chars_keeps_short_text_untouched() {
        assert_eq!(
            truncate_chars("短い".to_string(), 10),
            ("短い".to_string(), false)
        );
    }

    #[test]
    fn test_truncate_chars_cuts_on_char_boundary_for_multibyte_text() {
        // 絵文字(4 バイト)と日本語(3 バイト)が混ざっても、文字数で切って UTF-8 を壊さない。
        let text = "あい😀う😀え".to_string();
        let (cut, truncated) = truncate_chars(text, 4);
        assert_eq!(cut, "あい😀う");
        assert!(truncated);
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_chars_exact_limit_is_not_truncated() {
        assert_eq!(
            truncate_chars("あいう".to_string(), 3),
            ("あいう".to_string(), false)
        );
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_listen_prefers_flag_over_env_over_default() {
        assert_eq!(
            parse_listen(
                &args(&["--replay", "--listen", "127.0.0.1:9000"]),
                Some("0.0.0.0:1".into())
            )
            .unwrap()
            .to_string(),
            "127.0.0.1:9000"
        );
        assert_eq!(
            parse_listen(&args(&["--replay"]), Some("127.0.0.1:9001".into()))
                .unwrap()
                .to_string(),
            "127.0.0.1:9001"
        );
        assert_eq!(
            parse_listen(&args(&["--replay"]), None)
                .unwrap()
                .to_string(),
            DEFAULT_LISTEN
        );
    }

    #[test]
    fn test_parse_listen_rejects_invalid_address_and_missing_value() {
        assert!(parse_listen(&args(&["--replay", "--listen", "not-an-addr"]), None).is_err());
        assert!(parse_listen(&args(&["--replay", "--listen"]), None).is_err());
        assert!(parse_listen(&args(&["--replay"]), Some("bad".into())).is_err());
    }

    #[test]
    fn test_max_concurrency_from_env_value() {
        assert_eq!(max_concurrency_from(None), DEFAULT_MAX_CONCURRENCY);
        assert_eq!(max_concurrency_from(Some("2".into())), 2);
        assert_eq!(
            max_concurrency_from(Some("0".into())),
            DEFAULT_MAX_CONCURRENCY
        );
        assert_eq!(
            max_concurrency_from(Some("abc".into())),
            DEFAULT_MAX_CONCURRENCY
        );
    }

    #[tokio::test]
    async fn test_serve_answers_over_real_http_and_stops_on_shutdown() {
        let f = StubFactory::new(Behavior::Reply("本物のHTTP".to_string()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, AppState::new(f, 2), async {
            let _ = rx.await;
        }));

        let client = reqwest::Client::new();
        let health = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
        let res = client
            .post(format!("http://{addr}/v1/replay"))
            .json(&serde_json::json!({"model": "google/gemini-3.1-pro", "prompt": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.json::<serde_json::Value>().await.unwrap()["response"],
            "本物のHTTP"
        );

        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown で止まる")
            .unwrap()
            .unwrap();
    }

    // ---- トレースの連結(shinasadame の `traceparent` を受け取る) ----

    #[cfg(feature = "otel")]
    mod trace_context {
        use super::*;
        use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
        const PARENT_SPAN_ID: &str = "b7ad6b7169203331";
        const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

        fn use_w3c_propagator() {
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        }

        #[test]
        fn test_extract_parent_context_reads_traceparent() {
            use_w3c_propagator();
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("traceparent", TRACEPARENT.parse().unwrap());

            let cx = extract_parent_context(&headers);
            let span = cx.span();
            let sc = span.span_context();

            assert!(sc.is_valid() && sc.is_remote());
            assert_eq!(sc.trace_id().to_string(), TRACE_ID);
            assert_eq!(sc.span_id().to_string(), PARENT_SPAN_ID);
        }

        /// Python の OTel SDK は W3C Trace Context Level 2 の「ランダムなトレース ID」フラグ(0x02)も
        /// 立てて `-03` を送る。Rust の propagator は version 00 で flags > 2 を不正として捨てるため、
        /// 正規化しないと親が失われて別トレースになる(実際に起きた不具合)。
        #[test]
        fn test_extract_parent_context_accepts_level2_random_flag() {
            use_w3c_propagator();
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "traceparent",
                format!("00-{TRACE_ID}-{PARENT_SPAN_ID}-03")
                    .parse()
                    .unwrap(),
            );

            let cx = extract_parent_context(&headers);
            let span = cx.span();
            let sc = span.span_context();

            assert!(sc.is_valid(), "-03 の traceparent が捨てられている");
            assert_eq!(sc.trace_id().to_string(), TRACE_ID);
            assert_eq!(sc.span_id().to_string(), PARENT_SPAN_ID);
            assert!(sc.is_sampled());
        }

        #[test]
        fn test_extract_parent_context_keeps_unsampled_flag() {
            use_w3c_propagator();
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "traceparent",
                format!("00-{TRACE_ID}-{PARENT_SPAN_ID}-02")
                    .parse()
                    .unwrap(),
            );
            let cx = extract_parent_context(&headers);
            let span = cx.span();
            assert!(span.span_context().is_valid());
            assert!(
                !span.span_context().is_sampled(),
                "sampled でないものは sampled にしない"
            );
        }

        #[test]
        fn test_extract_parent_context_ignores_malformed_traceparent() {
            use_w3c_propagator();
            for bad in ["garbage", "00-zz-yy-01", "00-abc", ""] {
                let mut headers = axum::http::HeaderMap::new();
                headers.insert("traceparent", bad.parse().unwrap());
                assert!(
                    !extract_parent_context(&headers)
                        .span()
                        .span_context()
                        .is_valid(),
                    "{bad:?} から親を作ってはいけない"
                );
            }
        }

        #[tokio::test]
        async fn test_request_span_continues_a_python_style_trace() {
            use_w3c_propagator();
            let (exporter, provider, _guard) = capture_spans();

            let f = StubFactory::new(Behavior::Reply("ok".to_string()));
            let request = Request::builder()
                .method("POST")
                .uri("/v1/replay")
                .header("content-type", "application/json")
                .header("traceparent", format!("00-{TRACE_ID}-{PARENT_SPAN_ID}-03"))
                .body(Body::from(OK_BODY))
                .unwrap();
            let res = app(f, 4).oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();
            let server = span_named(&spans, "replay.http");
            assert_eq!(server.span_context.trace_id().to_string(), TRACE_ID);
            assert_eq!(server.parent_span_id.to_string(), PARENT_SPAN_ID);
        }

        #[test]
        fn test_extract_parent_context_without_header_is_empty() {
            use_w3c_propagator();
            let cx = extract_parent_context(&axum::http::HeaderMap::new());
            assert!(!cx.span().span_context().is_valid());
        }

        #[tokio::test]
        async fn test_request_span_continues_the_incoming_trace() {
            use_w3c_propagator();
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
            // current_thread のテストランタイムなので、await をまたいでも同じスレッドの既定が効く
            let _guard = tracing::subscriber::set_default(subscriber);

            let f = StubFactory::new(Behavior::Reply("ok".to_string()));
            let request = Request::builder()
                .method("POST")
                .uri("/v1/replay")
                .header("content-type", "application/json")
                .header("traceparent", TRACEPARENT)
                .body(Body::from(OK_BODY))
                .unwrap();
            let res = app(f, 4).oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();
            let server = spans
                .iter()
                .find(|s| s.name == "replay.http")
                .expect("リクエストのスパンが出力される");
            assert_eq!(server.span_context.trace_id().to_string(), TRACE_ID);
            assert_eq!(server.parent_span_id.to_string(), PARENT_SPAN_ID);
        }

        #[tokio::test]
        async fn test_healthz_is_not_traced() {
            use_w3c_propagator();
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
            let _guard = tracing::subscriber::set_default(subscriber);

            let f = StubFactory::new(Behavior::Reply("ok".to_string()));
            // k8s のプローブは頻繁に来るので、トレースに混ぜない(traceparent が付いていても)
            let request = Request::builder()
                .uri("/healthz")
                .header("traceparent", TRACEPARENT)
                .body(Body::empty())
                .unwrap();
            let res = app(f, 4).oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();
            assert!(
                spans.iter().all(|s| s.name != "replay.http"),
                "/healthz のスパンが出力されている: {:?}",
                spans.iter().map(|s| s.name.to_string()).collect::<Vec<_>>()
            );
        }

        /// メモリ上の exporter へスパンを集める(戻り値のガードを保持している間だけ有効)。
        fn capture_spans() -> (
            InMemorySpanExporter,
            SdkTracerProvider,
            tracing::subscriber::DefaultGuard,
        ) {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
            (
                exporter,
                provider,
                tracing::subscriber::set_default(subscriber),
            )
        }

        fn span_named<'a>(
            spans: &'a [opentelemetry_sdk::trace::SpanData],
            name: &str,
        ) -> &'a opentelemetry_sdk::trace::SpanData {
            spans.iter().find(|s| s.name == name).unwrap_or_else(|| {
                panic!(
                    "スパン {name} が無い: {:?}",
                    spans.iter().map(|s| s.name.to_string()).collect::<Vec<_>>()
                )
            })
        }

        fn has_attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> bool {
            span.attributes.iter().any(|kv| kv.key.as_str() == key)
        }

        fn attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> String {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
                .unwrap_or_else(|| panic!("属性 {key} が無い"))
        }

        /// `err` は、span の status を Error にし、説明に Debug 表記のエラーを入れる。
        fn is_error_status(span: &opentelemetry_sdk::trace::SpanData, contains: &str) -> bool {
            matches!(&span.status, opentelemetry::trace::Status::Error { description }
                if description.contains(contains))
        }

        /// `ret` の戻り値は、スパンのイベントの `return` 属性として記録される。
        fn event_attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
            span.events.iter().find_map(|e| {
                e.attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == key)
                    .map(|kv| kv.value.to_string())
            })
        }

        #[tokio::test]
        async fn test_handler_and_validate_are_instrumented_with_return_values() {
            use_w3c_propagator();
            let (exporter, provider, _guard) = capture_spans();

            let f = StubFactory::new(Behavior::Reply("トレースで見たい応答".to_string()));
            let (status, _) = post_json(app(f, 4), OK_BODY).await;
            assert_eq!(status, StatusCode::OK);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();

            // 分岐のある関数のスパンが出る
            let validate = span_named(&spans, "validate");
            assert_eq!(attr(validate, "model"), "google/gemini-3.1-pro");
            // 戻り値(有効なタイムアウト)がイベントに残る
            assert!(
                event_attr(validate, "return").is_some_and(|v| v.contains("120")),
                "validate の戻り値が記録されていない"
            );

            // handler は、デバッグで見たいプロンプトと応答を属性に持つ
            let handler = span_named(&spans, "replay_handler");
            assert_eq!(attr(handler, "model"), "google/gemini-3.1-pro");
            assert_eq!(attr(handler, "prompt"), "続きを書いて");
            assert_eq!(attr(handler, "response"), "トレースで見たい応答");
            assert!(has_attr(handler, "elapsed_ms"));
            assert_eq!(attr(handler, "truncated"), "false");
        }

        #[tokio::test]
        async fn test_error_paths_record_the_classified_error() {
            use_w3c_propagator();
            let (exporter, provider, _guard) = capture_spans();

            let f = StubFactory::new(Behavior::Fail("HTTP 401 Unauthorized".to_string()));
            let (status, _) = post_json(app(f, 4), OK_BODY).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();

            let classify = span_named(&spans, "classify_llm_error");
            assert!(
                event_attr(classify, "return").is_some_and(|v| v.contains("Auth")),
                "分類結果(Auth)が戻り値として記録されていない"
            );
            // handler のエラーは span の status(Error)と `exception` イベントに残る
            let handler = span_named(&spans, "replay_handler");
            assert!(
                is_error_status(handler, "Auth"),
                "handler の status が Error になっていない"
            );
            assert!(
                event_attr(handler, "exception.message").is_some_and(|v| v.contains("Auth")),
                "handler のエラーが記録されていない"
            );
        }

        #[tokio::test]
        async fn test_validation_failure_records_the_error_return() {
            use_w3c_propagator();
            let (exporter, provider, _guard) = capture_spans();

            let f = StubFactory::new(Behavior::Reply("ok".to_string()));
            let body = r#"{"model":"nosuch/m","prompt":"x"}"#;
            let (status, _) = post_json(app(f, 4), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();
            let validate = span_named(&spans, "validate");
            assert!(is_error_status(validate, "BadRequest"));
            assert!(
                event_attr(validate, "exception.message").is_some_and(|v| v.contains("nosuch/m")),
                "検証エラーの内容が記録されていない"
            );
            // 検証に失敗したので、handler はエラー終了になる
            assert!(is_error_status(
                span_named(&spans, "replay_handler"),
                "BadRequest"
            ));
        }

        #[test]
        fn test_pure_helpers_are_instrumented_with_return_values() {
            use_w3c_propagator();
            let (exporter, provider, _guard) = capture_spans();

            let _ = max_concurrency_from(Some("2".to_string()));
            let _ = parse_listen(&[], Some("127.0.0.1:9".to_string()));
            let _ = truncate_chars("あいう".to_string(), 2);
            let _ = ErrorKind::Timeout.status();

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();

            assert_eq!(
                event_attr(span_named(&spans, "max_concurrency_from"), "return").as_deref(),
                Some("2")
            );
            assert!(
                event_attr(span_named(&spans, "parse_listen"), "return")
                    .is_some_and(|v| v.contains("127.0.0.1:9"))
            );
            assert!(
                event_attr(span_named(&spans, "status"), "return")
                    .is_some_and(|v| v.contains("504"))
            );
            // truncate_chars は本文が戻り値なので、`ret` は使わず、切り詰めたかだけを属性にする
            let truncate = span_named(&spans, "truncate_chars");
            assert_eq!(attr(truncate, "truncated"), "true");
            assert!(
                !format!("{truncate:?}").contains("あい"),
                "本文がスパンに載っている"
            );
        }

        #[tokio::test]
        async fn test_request_without_traceparent_starts_its_own_trace() {
            use_w3c_propagator();
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
            let _guard = tracing::subscriber::set_default(subscriber);

            let f = StubFactory::new(Behavior::Reply("ok".to_string()));
            let request = Request::builder()
                .method("POST")
                .uri("/v1/replay")
                .header("content-type", "application/json")
                .body(Body::from(OK_BODY))
                .unwrap();
            app(f, 4).oneshot(request).await.unwrap();

            let _ = provider.force_flush();
            let spans = exporter.get_finished_spans().unwrap();
            let server = spans.iter().find(|s| s.name == "replay.http").unwrap();
            assert_ne!(server.span_context.trace_id().to_string(), TRACE_ID);
            assert_eq!(server.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        }
    }
}
