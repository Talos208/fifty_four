use async_trait::async_trait;
use bitflags::bitflags;
use derive_more::Display;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ReasoningEffort, ServiceTier, Tool,
    ToolCall, ToolResponse, Verbosity,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ModelName, ServiceTarget};

bitflags! {
    /// モデルが対応している機能のビットフラグ
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ModelCapability: u32 {
        /// JsonSpec による構造化出力（genai の response_format が有効）
        const STRUCTURED_OUTPUT = 1 << 0;
        /// Tool calling
        const TOOL_CALLING      = 1 << 1;
        /// reasoning_effort を送ってよい
        /// (xAI の grok-4.20-0309-* のように reasoning 深度がスナップショットに
        ///  固定されているモデルは非対応。送ると 400 "does not support parameter
        ///  reasoningEffort" が返る)
        const REASONING_EFFORT  = 1 << 2;
        /// stop シーケンスを送ってよい
        /// (一部プロバイダの reasoning モデルは `stop` パラメータ自体が非対応)
        const STOP_SEQUENCES    = 1 << 3;
    }
}
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt::{self, Debug, Display};

/// LLMプロバイダを表すenum
#[derive(Debug, Clone)]
pub enum Provider {
    Google(String),
    OpenAI(String),
    Anthropic(String),
    XAi(String),
    LMStudio(String, Option<String>),
    Cloudflare(String),

    Undefined,
}

impl Provider {
    /// 文字列からProviderを生成する
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "google" => Ok(Provider::Google("gemini-3.1-pro-preview".to_string())),
            "openai" => Ok(Provider::OpenAI("gpt-5.3".to_string())),
            "anthropic" => Ok(Provider::Anthropic("claude-4.6-sonnet".to_string())),
            "xai" => Ok(Provider::XAi("grok-4.5".to_string())),
            "lmstudio" => Ok(Provider::LMStudio(
                "qwen3.5-2b".to_string(),
                Some("http://localhost:1234/v1/".to_string()),
            )),
            "cloudflare" => Ok(Provider::Cloudflare(
                "@cf/meta/llama-3.1-8b-instruct".to_string(),
            )),
            _ => Err(format!("Unsupported provider: {}", s)),
        }
    }

    #[allow(unused)]
    pub fn from_name(name: &str) -> Result<Self, String> {
        let (mut prov, model) = if let Some((prov, model)) = name.split_once('/') {
            (prov, model)
        } else {
            (
                if let Some((token, _)) = name.split_once('-') {
                    match token {
                        "gemini" => "google",
                        "gpt" => "openai",
                        "claude" => "anthropic",
                        "grok" => "xai",
                        _ => "lmstudio",
                    }
                } else {
                    name
                },
                name,
            )
        };

        let mut url = "localhost:1234";
        if prov.starts_with("lmstudio")
            && let Some((p, u)) = prov.split_once('@')
        {
            prov = p;
            url = u;
        }

        match prov {
            "google" => Ok(Provider::Google(model.to_string())),
            "openai" => Ok(Provider::OpenAI(model.to_string())),
            "anthropic" => Ok(Provider::Anthropic(model.to_string())),
            "xai" => Ok(Provider::XAi(model.to_string())),
            "lmstudio" => Ok(Provider::LMStudio(model.to_string(), Some(url.to_string()))),
            "cloudflare" => Ok(Provider::Cloudflare(model.to_string())),
            _ => Err(format!("Unsupported provider: {}", name)),
        }
    }

    /// プロバイダと実際に使用するモデル名から capability を導出する。
    ///
    /// LMStudio はローカルLLM前提でモデルが多様なため常に空(未対応)とし、
    /// 呼び出し側で明示的に `capabilities` を設定する運用とする。
    /// それ以外のプロバイダは自社モデルのラインナップが既知のため、
    /// プロバイダ単位で決め打ちできるものはそうしつつ、Cloudflare Workers AI
    /// のようにモデルごとに tool calling 対応が分かれるプロバイダは
    /// モデル名も加味して判定する。
    pub fn default_capabilities(&self, model: &str) -> ModelCapability {
        match self {
            // Anthropic の構造化出力(output_config.format)は genai 0.6.5 の
            // Anthropic adapter が ChatResponseFormat::JsonSpec を通じて対応済み。
            // reasoning_effort / stop はどちらも各社ドキュメント上サポート対象。
            Provider::Google(_) | Provider::OpenAI(_) | Provider::Anthropic(_) => {
                ModelCapability::STRUCTURED_OUTPUT
                    | ModelCapability::TOOL_CALLING
                    | ModelCapability::REASONING_EFFORT
                    | ModelCapability::STOP_SEQUENCES
            }
            Provider::XAi(_) => Self::xai_capabilities(model),
            Provider::Cloudflare(_) => Self::cloudflare_capabilities(model),
            Provider::LMStudio(..) | Provider::Undefined => ModelCapability::empty(),
        }
    }

    /// Cloudflare Workers AI のモデル一覧は function calling 対応がモデル依存のため、
    /// モデル名に含まれるファミリー名から簡易判定する。
    /// 対応が確実でないモデルは安全側(空)に倒す。
    fn cloudflare_capabilities(model: &str) -> ModelCapability {
        let m = model.to_lowercase();
        let supports_tool_calling = ["instruct", "hermes", "qwen", "mistral"]
            .iter()
            .any(|kw| m.contains(kw));
        if supports_tool_calling {
            ModelCapability::TOOL_CALLING
        } else {
            ModelCapability::empty()
        }
    }

    /// xAI (Grok) はモデルごとに reasoning_effort の対応が大きく割れているため、
    /// モデル名から判定する。2026-08 時点の x.ai 公式ドキュメント準拠:
    /// - `grok-4.20-0309-reasoning` / `-non-reasoning`: reasoning 深度がスナップショット
    ///   に固定されており reasoning_effort 自体が非対応(送ると 400)。
    /// - `grok-4.20-multi-agent`: reasoning_effort は「エージェント数(4 or 16)」の指定に
    ///   転用されており、値の意味が他モデルと異なる点に注意。
    /// - `grok-4.3` / `grok-4.5`: 通常どおり reasoning 深度として対応(grok-4.5 は
    ///   none 不可＝無効化できない)。
    ///
    /// 未知のモデル名は安全側に倒し reasoning_effort を送らない
    /// (静的表が古くなった場合は 400 の自己修復リトライ(`unsupported_param_from_error_body`)で吸収する)。
    ///
    /// 判定順序に注意: "grok-4.20-multi-agent" は "grok-4.20" のプレフィックスにも
    /// マッチするため、multi-agent の判定を先に行う。
    fn xai_capabilities(model: &str) -> ModelCapability {
        let base = ModelCapability::STRUCTURED_OUTPUT | ModelCapability::TOOL_CALLING;
        let m = model.to_lowercase();
        if m.contains("grok-4.20-multi-agent") {
            base | ModelCapability::REASONING_EFFORT
        } else if m.contains("grok-4.20") {
            base
        } else if m.contains("grok-4.3") || m.contains("grok-4.5") {
            base | ModelCapability::REASONING_EFFORT
        } else {
            base
        }
    }

    /// 正規化 effort (0.0..=1.0) をこのプロバイダ・モデルの ReasoningEffort 段階へ
    /// 写像する。0.0 (以下) または reasoning 非対応 => None。1.0 => 最上位段。
    pub fn map_reasoning(&self, model: &str, norm: f64) -> ReasoningEffort {
        use ReasoningEffort::*;
        let m = model.to_lowercase();
        // 低→高 の順。空 = reasoning 非対応。
        let ladder: &[ReasoningEffort] = match self {
            Provider::OpenAI(_) => &[Low, Medium, High, XHigh],
            Provider::Anthropic(_) => &[Low, Medium, High, XHigh, Max],
            // `gemini-3-pro-preview`(`.1` 無し)は thinkingLevel が low/high の2段のみで、
            // medium を送ると 400 "Thinking level MEDIUM is not supported for this model."
            // が返る。`gemini-3.1-pro-preview` 等の `.1` 系や旧 gemini-2.5系は
            // low/medium/high に対応しているため、プレフィックス一致で bare な
            // "gemini-3-pro-preview" のみ2段のラダーに倒す。
            Provider::Google(_) if m.starts_with("gemini-3-pro-preview") => &[Low, High],
            Provider::Google(_) => &[Low, Medium, High],
            Provider::XAi(_) if m.contains("grok-4.20-multi-agent") => {
                // multi-agent モデルでは reasoning 深度ではなく
                // エージェント数(4 or 16)の指定に転用される。
                &[Low, Medium, High, XHigh]
            }
            Provider::XAi(_) if m.contains("grok-4.20") => &[],
            Provider::XAi(_) if m.contains("grok-4.3") || m.contains("grok-4.5") => {
                &[Low, Medium, High]
            }
            Provider::XAi(_) => &[],
            Provider::Cloudflare(_) | Provider::LMStudio(..) | Provider::Undefined => &[],
        };
        let n = norm.clamp(0.0, 1.0);
        if n <= 0.0 || ladder.is_empty() {
            ReasoningEffort::None
        } else {
            // 1.0 で必ず末尾(最上位)に到達するよう ceil でインデックス化
            let idx = ((n * ladder.len() as f64).ceil() as usize).clamp(1, ladder.len()) - 1;
            ladder[idx].clone()
        }
    }

    #[allow(unused)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Google(s) => write!(f, "Google({})", s),
            Provider::OpenAI(s) => write!(f, "OpenAI({})", s),
            Provider::Anthropic(s) => write!(f, "Anthropic({})", s),
            Provider::XAi(s) => write!(f, "XAi({})", s),
            Provider::LMStudio(s, _) => write!(f, "LMStudio({})", s),
            Provider::Cloudflare(s) => write!(f, "Cloudflare({})", s),
            Provider::Undefined => write!(f, "Undefined"),
        }
    }

    fn clone(&self) -> Self {
        match self {
            Provider::Google(s) => Provider::Google(s.clone()),
            Provider::OpenAI(s) => Provider::OpenAI(s.clone()),
            Provider::Anthropic(s) => Provider::Anthropic(s.clone()),
            Provider::XAi(s) => Provider::XAi(s.clone()),
            Provider::LMStudio(s, _) => Provider::LMStudio(s.clone(), None),
            Provider::Cloudflare(s) => Provider::Cloudflare(s.clone()),
            _ => Provider::Undefined,
        }
    }
}

/// xAI 等が返す 400 のエラー本文から「対応していないパラメータ名」を抽出する。
///
/// 例: `"Model grok-4.20-0309-reasoning does not support parameter reasoningEffort."`
/// → `Some("reasoning_effort")`
///
/// 静的な capability 表(`Provider::xai_capabilities` 等)が実際の API 仕様に
/// 追いついていない場合の保険。パラメータ名は camelCase で返ってくるため
/// snake_case に正規化する。
fn unsupported_param_from_error_body(body: &str) -> Option<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // JSON エラー本文では引用符がバックスラッシュエスケープされて
        // `\"stop\"` のように来ることがあるため、`\`・`"` を任意個読み飛ばす。
        regex::Regex::new(r#"does not support parameter[\s\\"]+([A-Za-z][A-Za-z0-9_]*)"#).unwrap()
    });
    let raw = RE.captures(body)?.get(1)?.as_str();
    Some(camel_to_snake(raw))
}

/// camelCase を snake_case へ正規化する(既に snake_case の場合はそのまま)。
fn camel_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 正規化済みパラメータ名に対応する `ChatOptions` のフィールドを落とす。
/// 実際に値が入っていて外せた場合のみ `true` を返す(既に空なら再送しても
/// 同じ 400 が繰り返されるだけなので、無限リトライを避けるため `false`)。
fn strip_unsupported_option(options: &mut ChatOptions, param: &str) -> bool {
    match param {
        "reasoning_effort" => options.reasoning_effort.take().is_some(),
        "stop" | "stop_sequences" => {
            if options.stop_sequences.is_empty() {
                false
            } else {
                options.stop_sequences.clear();
                true
            }
        }
        _ => false,
    }
}

/// LLMクライアントの機能を抽象化する非同期トレイト
///
/// このトレイトは、LLMとの対話（チャット）、プロンプトのキャッシュ、
/// そして新しいプロンプトの追加といった基本的な操作を定義します。
/// これにより、具体的なLLMプロバイダ（Google, OpenAIなど）の実装を
/// アプリケーションのコアロジックから切り離すことができます。
#[derive(Debug, Clone)]
#[allow(unused)]
pub enum Content {
    Text(String),
    CacheEntry(String),
}

impl Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Content::Text(s) => write!(f, "{}", s),
            Content::CacheEntry(s) => write!(f, "{}", s),
        }
    }
}

impl AsRef<String> for Content {
    fn as_ref(&self) -> &String {
        match self {
            Content::Text(s) => s,
            Content::CacheEntry(s) => s,
        }
    }
}

#[derive(Debug, Display)]
pub enum LlmError {
    #[display("LLM is not initialized")]
    NotInitialized,
    #[display("Cache \"{}\" is not found", key)]
    #[allow(unused)]
    CacheNotFound { key: String },
    #[display("LLM is busy, retry after {} seconds", retry_after)]
    LlmBusy { retry_after: u32 },
    #[display("Generation error: {}", message)]
    GenericError { message: String },
    #[display("JSON parse error: {}", message)]
    JsonParseError { message: String },
    #[display("Not implemented")]
    #[allow(unused)]
    NotImplemented,
}

impl std::error::Error for LlmError {}

#[async_trait]
#[allow(unused)]
pub trait LlmInterface: Send + Sync + std::fmt::Debug {
    //+ MaybeUninit {
    /// LLMとチャットセッションを実行する
    ///
    /// 現在のプロンプトを基にLLMにリクエストを送信し、応答を文字列として返します。
    async fn chat(&mut self) -> Result<String, LlmError>;

    async fn with_model(&mut self, model: &str) -> Result<String, LlmError>;

    /// 一時的なプロンプトを現在のセッションに追加する
    /// TODO Sessionを作ってそっちに持たせる
    /// このプロンプトは、次回の`chat`呼び出しでのみ使用され、その後クリアされます。
    fn add(&mut self, prompt: Content);

    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LlmClient {}", self.get_model())
    }

    async fn get_service_target(&self) -> String {
        "".to_string()
    }

    fn build_content(&self) -> String;

    fn get_model(&self) -> &str;

    /// キャッシュを初期化（クリア）する
    fn clear(&mut self);

    /// プロンプトを永続的なキャッシュに追加する
    ///
    /// このプロンプトは、`chat`が呼び出されるたびに再利用されます。
    fn cache(&mut self, prompt: Content) -> Result<String, LlmError>;

    fn fetch(&self, hash: &str) -> Option<&Content>;

    fn fetch_all(&self) -> Vec<Content>;

    fn remove(&mut self, hash: String);

    fn capabilities(&self) -> ModelCapability;

    // オプション設定
    fn max_tokens(&mut self, n: u32);
    fn temperature(&mut self, v: f64);
    fn top_p(&mut self, v: f64);
    fn stop_sequences(&mut self, seqs: Vec<String>);
    fn seed(&mut self, v: u64);
    fn reasoning_effort(&mut self, effort: ReasoningEffort);
    /// 正規化された reasoning/thinking effort を指定する (0.0..=1.0)。
    /// 0.0 = none、1.0 = そのプロバイダの最高 effort。範囲外は clamp。
    fn reasoning_level(&mut self, level: f64);

    /// リクエスト単位のオプション(max_tokens/temperature/reasoning_level 等)を
    /// 既定値へ戻す。呼び出し元がリクエストごとにオプションを設定し直す前に
    /// 呼び、前回リクエストの設定が失敗・キャンセル時に持ち越されるのを防ぐ。
    /// 既定実装は no-op(モックなどオプションを持たない実装向け)。
    fn reset_options(&mut self) {}
    fn response_format(&mut self, fmt: ChatResponseFormat);
    fn service_tier(&mut self, tier: ServiceTier);
    fn verbosity(&mut self, v: Verbosity);

    // Tool calling
    fn add_tool(&mut self, tool: Box<dyn LlmTool>);
    async fn respond_tool(&self, tools: &[ToolCall]) -> Result<ChatRequest, LlmError>;
}

#[derive(Debug)]
pub struct LlmClientBuilder {
    provider: Provider,
    model: Option<String>,
    url: Option<String>,
    tools: Vec<Box<dyn LlmTool>>,
    sys_prompt: String,
    /// 明示的に指定された capability。None の場合は provider のデフォルトを使う。
    capabilities: Option<ModelCapability>,
    /// Cloudflare Workers AI 用のアカウントID。エンドポイントURLの組み立てに使う。
    account_id: Option<String>,
}

impl LlmClientBuilder {
    /// LLMビルダーを構築する
    #[allow(unused)]
    fn new(_provider: Provider) -> LlmClientBuilder {
        LlmClientBuilder {
            provider: _provider.clone(),
            model: None,
            url: None,
            tools: vec![],
            sys_prompt: String::new(),
            capabilities: None,
            account_id: None,
        }
    }

    #[allow(unused)]
    pub fn from_name(name: &str) -> Self {
        LlmClientBuilder {
            provider: Provider::from_name(name).unwrap(),
            model: None,
            url: None,
            tools: vec![],
            sys_prompt: String::new(),
            capabilities: None,
            account_id: None,
        }
    }

    pub fn from_value(value: &serde_json::Value) -> Self {
        debug!("value: {:?}", value);
        let provider = Provider::from_str(value["provider"].as_str().unwrap()).unwrap();
        let capabilities = value
            .get("capabilities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().fold(ModelCapability::empty(), |acc, item| {
                    match item.as_str().unwrap_or("") {
                        "structured_output" => acc | ModelCapability::STRUCTURED_OUTPUT,
                        "tool_calling" => acc | ModelCapability::TOOL_CALLING,
                        "reasoning_effort" => acc | ModelCapability::REASONING_EFFORT,
                        "stop_sequences" => acc | ModelCapability::STOP_SEQUENCES,
                        _ => acc,
                    }
                })
            });
        LlmClientBuilder {
            provider,
            model: value.get("model").map(|v| v.as_str().unwrap().to_string()),
            url: value.get("url").map(|v| v.as_str().unwrap().to_string()),
            tools: vec![],
            sys_prompt: String::new(),
            capabilities,
            account_id: value
                .get("account_id")
                .and_then(|v| v.as_str())
                .map(String::from),
        }
    }

    #[allow(dead_code)]
    pub fn model(&mut self, model: &str) -> &Self {
        self.model = Some(model.to_string());
        self
    }

    #[allow(dead_code)]
    pub fn url(&mut self, url: &str) -> &Self {
        self.url = Some(url.to_string());
        self
    }

    #[allow(unused)]
    pub fn add_tool(mut self, tool: Box<dyn LlmTool>) -> Self {
        self.tools.push(tool);
        debug!("Tool added to builder {:?}", self.tools);

        self
    }

    pub fn sys_prompt(mut self, prompt: Option<String>) -> Self {
        if let Some(p) = prompt {
            self.sys_prompt = p;
        }

        self
    }

    pub fn build(self) -> Box<dyn LlmInterface> {
        let mut u_from_prov = None;
        let (auth, kind, mdl_name) = match &self.provider {
            Provider::Google(mdl) => (
                AuthData::FromEnv("GEMINI_API_KEY".to_string()),
                AdapterKind::Gemini,
                mdl,
            ),
            Provider::OpenAI(mdl) => (
                AuthData::FromEnv("OPENAI_API_KEY".to_string()),
                AdapterKind::OpenAI,
                mdl,
            ),
            Provider::Anthropic(mdl) => (
                AuthData::FromEnv("ANTHROPIC_API_KEY".to_string()),
                AdapterKind::Anthropic,
                mdl,
            ),
            Provider::XAi(mdl) => (
                AuthData::FromEnv("XAI_API_KEY".to_string()),
                AdapterKind::Xai,
                mdl,
            ),
            Provider::LMStudio(mdl, u) => {
                u_from_prov = u.clone();
                (
                    AuthData::Key("".to_string()), // No authentication required for local LMStudio
                    AdapterKind::Anthropic,
                    mdl,
                )
            }
            Provider::Cloudflare(mdl) => {
                let account = self.account_id.as_deref().unwrap_or_else(|| {
                    warn!(
                        "Cloudflare provider requires account_id; endpoint will be invalid without it"
                    );
                    ""
                });
                u_from_prov = Some(format!(
                    "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/",
                    account
                ));
                (
                    AuthData::FromEnv("CLOUDFLARE_API_TOKEN".to_string()),
                    AdapterKind::OpenAI,
                    mdl,
                )
            }
            _ => panic!("Unsupported provider"),
        };

        let mdl_name = self.model.as_ref().unwrap_or(mdl_name);
        let model_iden = ModelIden {
            adapter_kind: kind,
            model_name: ModelName::from(mdl_name),
        };
        let endpoint = if let Some(url) = self.url.clone() {
            Endpoint::from_owned(url)
        } else if let Some(url) = u_from_prov {
            Endpoint::from_owned(url)
        } else {
            Endpoint::from_static("")
        };

        let proc = ServiceTargetResolver::from_resolver_fn(
            |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                let e = if endpoint.base_url().is_empty() {
                    target.endpoint
                } else {
                    endpoint
                };
                debug!(
                    "ServiceTarget ( endpoint:{:?}, auth:{:?}, model:{:?} )",
                    e, auth, model_iden
                );
                Ok(ServiceTarget {
                    endpoint: e,
                    auth,
                    model: model_iden,
                })
            },
        );

        let inner_client = genai::Client::builder()
            .with_service_target_resolver(proc)
            .build();

        let capabilities = self
            .capabilities
            .unwrap_or_else(|| self.provider.default_capabilities(mdl_name));

        Box::new(LlmClient {
            provider: self.provider.clone(),
            inner_client,
            model: mdl_name.clone(),
            cache: HashMap::new(),
            prompts: vec![],
            options: ChatOptions::default(),
            tools: HashMap::from_iter(self.tools.into_iter().map(|t| {
                let n = t.name().to_string();
                (n, t)
            })),
            sys_prompt: self.sys_prompt,
            capabilities,
        })
    }
}

/// Toolcall 関連
#[async_trait]
pub trait LlmTool: Send + Sync + Debug {
    fn schema(&self) -> serde_json::Value;
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    async fn invoke(&self, args: &serde_json::Map<String, Value>) -> Result<String, LlmError>;
}

/// `LlmInterface`トレイトの汎用的な実装
///
/// `genai::Client`を内部的に利用し、特定のLLMプロバイダに依存しない形で
/// チャット機能を提供します。プロンプトの管理には`Cache`トレイトを利用します。
#[derive(Debug)]
pub struct LlmClient {
    provider: Provider,
    /// `genai`ライブラリのコアクライアント
    inner_client: Client,
    options: ChatOptions,
    /// 使用するLLMのモデル名
    model: String,
    /// プロンプトを永続的に保持するためのキャッシュ
    cache: HashMap<String, Content>,
    /// 現在のチャットセッションでのみ使用される一時的なプロンプト
    prompts: Vec<Content>,
    /// 利用可能なツール
    tools: HashMap<String, Box<dyn LlmTool>>,
    /// システムプロンプト
    sys_prompt: String,
    /// モデルが対応している機能
    capabilities: ModelCapability,
}

#[async_trait]
impl LlmInterface for LlmClient {
    async fn chat(&mut self) -> Result<String, LlmError> {
        let model = self.model.clone();
        self.with_model(model.as_str()).await
    }

    fn capabilities(&self) -> ModelCapability {
        self.capabilities
    }

    fn add_tool(&mut self, tool: Box<dyn LlmTool>) {
        self.tools.insert(tool.as_ref().name().to_string(), tool);
        debug!("Tool added to backend {:?}", self.tools.keys());
    }

    async fn with_model(&mut self, model: &str) -> Result<String, LlmError> {
        // TODO AGENTS.mdをfrom_system()で投入
        // 一時プロンプトはここで消費する(take)。この時点で chat_req へ焼き込まれるため、
        // 以降の送信失敗やキャンセル(このasync fnのfutureがdropされる場合)があっても
        // 次回呼び出しへ持ち越さない。以前は成功時のみ self.prompts.clear() していたため、
        // 補完リクエストが連打でキャンセルされるとプロンプトが前回分ごと累積し続けていた。
        let prompts = std::mem::take(&mut self.prompts);

        // chat_req はツール呼び出しの往復をまたいで会話履歴として保持・追記する。
        // (LLM API はステートレスなので、毎リクエストに全履歴を送る必要がある)
        let mut chat_req = ChatRequest::from_system(&self.sys_prompt)
            .append_messages(self.fetch_all().iter().filter_map(|c| {
                Some(ChatMessage::system(match c {
                    Content::Text(s) => s,
                    Content::CacheEntry(h) => self.fetch(h)?.as_ref(),
                }))
            }))
            .append_messages(prompts.iter().filter_map(|c| {
                Some(ChatMessage::user(match c {
                    Content::Text(s) => s,
                    Content::CacheEntry(h) => self.fetch(h)?.as_ref(),
                }))
            }))
            // Tool call 準備
            .with_tools(self.tools.values().map(|t| {
                Tool::new(t.name())
                    .with_description(t.description())
                    .with_schema(t.schema())
            }));

        // 静的な capability 表が古くなっていた場合の保険。400 の本文に
        // "does not support parameter <name>" があれば、そのパラメータを
        // options から落として1回だけ再送する(同じパラメータは二度と外さない
        // ので、想定外の応答が来ても無限ループにはならない)。
        let mut stripped_params: std::collections::HashSet<String> = std::collections::HashSet::new();

        let content = loop {
            let res = self
                .inner_client
                .exec_chat(model, chat_req.clone(), Some(&self.options))
                .await;

            let Ok(response) = res else {
                let err = res.unwrap_err();
                match err {
                    genai::Error::WebModelCall {
                        model_iden: _,
                        webc_error: ref e,
                    } => match e {
                        genai::webc::Error::ResponseFailedStatus {
                            status,
                            body: _,
                            headers,
                        } if *status == http::StatusCode::SERVICE_UNAVAILABLE => {
                            error!("Web error{:?} {:?}", status, headers);
                            let after = headers
                                .get("Retry-After")
                                .map(|v| v.to_str().unwrap_or("0").parse::<u32>().unwrap())
                                .unwrap_or(0u32);
                            return Err(LlmError::LlmBusy { retry_after: after });
                        }
                        genai::webc::Error::ResponseFailedStatus { status, body, .. } => {
                            if let Some(param) = unsupported_param_from_error_body(body)
                                && !stripped_params.contains(&param)
                                && strip_unsupported_option(&mut self.options, &param)
                            {
                                warn!(
                                    "model {} ({:?}) rejected parameter '{}'; retrying without it. body={}",
                                    self.model, status, param, body
                                );
                                stripped_params.insert(param);
                                continue;
                            }
                            error!("Web error{:?} body={}", status, body);
                        }
                        _ => {
                            error!("{:?}", e)
                        }
                    },
                    genai::Error::ChatResponseGeneration {
                        model_iden: _,
                        request_payload: _,
                        response_body: _,
                        cause: ref c,
                    } => {
                        let msg = format!("{:?}", c.clone());
                        error!("{}", msg);
                        return Err(LlmError::GenericError { message: msg });
                    }
                    _ => {
                        error!("{:?}", err)
                    }
                };
                return Err(LlmError::GenericError {
                    message: err.to_string(),
                });
            };

            // prompts は with_model 冒頭で既に take 済み。キャッシュ(fetch_all側)は保存されたまま。

            if let Some(reason) = response.reasoning_content.as_ref() {
                debug!("reasoning: {}", reason)
            }

            if response.tool_calls().is_empty() {
                // オプションのリセットは最終応答の確定時のみ。ツール往復の途中で
                // リセットすると response_format / temperature 等が2周目以降で消える。
                self.options = ChatOptions::default();
                break response.content.texts().join("");
            }

            // Toolcallが無くなるまでループ。
            // 置き換えではなく履歴へ追記する: system・元の user プロンプト・tools 宣言を
            // 保ったまま Assistant(functionCall) → User(functionResponse) を末尾に足す。
            // (まっさらなリクエストで functionCall から始めると Gemini が
            //  "function call turn must come after a user turn" の 400 を返す)
            let tc = response.into_tool_calls();
            match self.respond_tool(tc.as_slice()).await {
                Ok(follow) => chat_req.messages.extend(follow.messages),
                Err(e) => {
                    return Result::Err(e);
                }
            }
        };

        Ok(content)
    }

    async fn respond_tool(&self, tool_calls: &[ToolCall]) -> Result<ChatRequest, LlmError> {
        let mut result_tc = vec![];
        let mut result_tr = vec![];

        for tc in tool_calls {
            // モデルが実際に発話した functionCall は未知ツールでも履歴に残す。
            // 黙って除外するとモデルの発話と履歴が食い違い、functionCall と
            // functionResponse の件数不一致や同じ未知ツールの再呼び出しを招く。
            result_tc.push(tc.clone());

            let Some(t) = self.tools.get(&tc.fn_name) else {
                warn!("unknown tool called: {}", tc.fn_name);
                result_tr.push(ToolResponse::new(
                    tc.call_id.clone(),
                    format!("Error: unknown tool {}", tc.fn_name),
                ));
                continue;
            };

            let arg = tc
                .fn_arguments
                .as_object()
                .cloned()
                .unwrap_or(Map::default());
            info!("tool call {}({:?})", t.name(), arg);
            match t.invoke(&arg).await {
                // ツールコールの結果を返す
                Ok(ret) => {
                    result_tr.push(ToolResponse::new(tc.call_id.clone(), ret));
                }
                Err(e) => {
                    // ツールコールのエラーも返す
                    warn!("Fail to tool call: {:?}", e);
                    result_tr.push(ToolResponse::new(
                        tc.call_id.clone(),
                        format!("Error: {}", e),
                    ));
                }
            }
        }

        // genai の From 実装で組む(ToolCall の doc が示す慣用パターン
        //  append_message(tool_calls).append_message(tool_response))。
        // From<Vec<ToolCall>> は先頭 ToolCall の thought_signatures を先頭の
        // ThoughtSignature パートへ hoist する。Gemini 3 の送信シリアライザは
        // functionCall の直前にある ThoughtSignature パートからしか署名を読まないため、
        // これを通さないとモデルが返した署名が欠落し
        //  "Function call is missing a thought_signature" の 400 になる。
        // From<Vec<ToolResponse>> は Tool ロールになるが、Gemini adapter は User/Tool
        // どちらも {"role":"user", functionResponse} に出力するため出力は不変。
        let chat_req = ChatRequest::from_messages(vec![])
            .append_message(ChatMessage::from(result_tc))
            .append_message(ChatMessage::from(result_tr));
        Ok(chat_req)
    }

    fn add(&mut self, prompt: Content) {
        self.prompts.push(prompt);
    }

    fn build_content(&self) -> String {
        self.fetch_all()
            .iter()
            .chain(self.prompts.iter())
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    fn get_model(&self) -> &str {
        &self.model
    }

    fn cache(&mut self, prompt: Content) -> Result<String, LlmError> {
        match prompt {
            Content::Text(s) => {
                let key = format!("{:016x}", farmhash::hash64(s.as_bytes()));
                self.cache.insert(key.clone(), Content::Text(s));
                Ok(key)
            }
            Content::CacheEntry(key) => {
                if self.cache.contains_key(&key) {
                    Ok(key.clone())
                } else {
                    Err(LlmError::CacheNotFound { key })
                }
            }
        }
    }

    fn fetch(&self, hash: &str) -> Option<&Content> {
        self.cache.get(hash)
    }

    fn fetch_all(&self) -> Vec<Content> {
        self.cache.values().cloned().collect()
    }

    fn clear(&mut self) {
        self.cache.clear();
    }

    fn remove(&mut self, hash: String) {
        self.cache.remove(&hash);
    }

    async fn get_service_target(&self) -> String {
        let st = self
            .inner_client
            .resolve_service_target(self.model.as_str())
            .await
            .unwrap();

        format!("{:?} {:?}", st.model, st.endpoint)
    }

    fn max_tokens(&mut self, n: u32) {
        self.options = std::mem::take(&mut self.options).with_max_tokens(n);
    }

    fn temperature(&mut self, v: f64) {
        self.options = std::mem::take(&mut self.options).with_temperature(v);
    }

    fn top_p(&mut self, v: f64) {
        self.options = std::mem::take(&mut self.options).with_top_p(v);
    }

    fn stop_sequences(&mut self, seqs: Vec<String>) {
        if !self.capabilities.contains(ModelCapability::STOP_SEQUENCES) {
            debug!(
                "model {} does not support stop_sequences; skipping",
                self.model
            );
            return;
        }
        self.options = std::mem::take(&mut self.options).with_stop_sequences(seqs);
    }

    fn seed(&mut self, v: u64) {
        self.options = std::mem::take(&mut self.options).with_seed(v);
    }

    fn reasoning_effort(&mut self, effort: ReasoningEffort) {
        if !self.capabilities.contains(ModelCapability::REASONING_EFFORT) {
            debug!(
                "model {} does not support reasoning_effort; skipping",
                self.model
            );
            return;
        }
        self.options = std::mem::take(&mut self.options).with_reasoning_effort(effort);
    }

    fn reasoning_level(&mut self, level: f64) {
        if !self.capabilities.contains(ModelCapability::REASONING_EFFORT) {
            debug!(
                "model {} does not support reasoning_effort; skipping (level={})",
                self.model, level
            );
            return;
        }
        // モデル名は builder 側で上書き済みの self.model を見る(self.provider が
        // 内部に保持する既定モデル名ではなく、実際に送信するモデル名で判定する)。
        let effort = self.provider.map_reasoning(&self.model, level);
        self.options = std::mem::take(&mut self.options).with_reasoning_effort(effort);
    }

    fn reset_options(&mut self) {
        self.options = ChatOptions::default();
    }

    fn response_format(&mut self, fmt: ChatResponseFormat) {
        self.options = std::mem::take(&mut self.options).with_response_format(fmt);
    }

    fn service_tier(&mut self, tier: ServiceTier) {
        self.options = std::mem::take(&mut self.options).with_service_tier(tier);
    }

    fn verbosity(&mut self, v: Verbosity) {
        self.options = std::mem::take(&mut self.options).with_verbosity(v);
    }
}

/// 設定 JSON から LLM クライアントを組み立て、`data/<sys_prompt_name>` を
/// システムプロンプトとして与える。
///
/// システムプロンプトの差し替えだけが違う複数の用途(LSP の執筆支援=`system.md`、
/// ACP のチャット=`system_chat.md`)があるため、アセット名を引数に取る。
/// アセットが見つからない場合はシステムプロンプト無しで動かし、警告だけ出す。
pub(crate) fn build_client(
    cfg: &serde_json::Value,
    sys_prompt_name: &str,
) -> Box<dyn LlmInterface> {
    let sys_prompt = crate::assets::load(sys_prompt_name);
    if sys_prompt.is_none() {
        warn!(
            "{} not found on disk nor in embedded assets; LLM runs without system prompt",
            sys_prompt_name
        );
    }
    LlmClientBuilder::from_value(cfg)
        .sys_prompt(sys_prompt)
        .build()
}

/// プロンプトの frontmatter で指定されたオプションを適用してから LLM を使う。
///
/// `slot` をロックし、`reset_options()` で前回の設定を捨ててから `option` を適用し、
/// `proc` に `&mut Box<dyn LlmInterface>` を渡す。`schema` が指定されていれば
/// capability に応じて構造化出力かプロンプト埋め込みへ振り分け、応答が JSON で
/// あることの検証(とコードフェンス剥がし)まで面倒を見る。
///
/// LSP ハンドラ(`Backend::use_llm_with_option`)と ACP エージェント(`crate::acp`)の
/// 双方から使うため、`Backend` に依存しない自由関数としてここに置いている。
pub(crate) async fn use_llm_with_option<F>(
    slot: &tokio::sync::Mutex<Option<Box<dyn LlmInterface>>>,
    option: HashMap<String, String>,
    proc: F,
) -> Result<String, LlmError>
where
    F: for<'b, 'a> AsyncFnOnce(&'b mut Box<dyn LlmInterface + 'a>) -> Result<String, LlmError>,
{
    use genai::chat::JsonSpec;

    debug!("Options {:?}", option);
    let mut ref_llm = slot.lock().await;

    let Some(llm) = ref_llm.as_mut() else {
        return Err(LlmError::NotInitialized);
    };

    // 前回リクエストの max_tokens/temperature/reasoning_level 等が
    // 失敗・キャンセル時に持ち越されないよう、適用前に既定値へ戻す。
    llm.reset_options();
    if let Some(v) = option.get("max_tokens")
        && let Ok(n) = v.parse::<u32>()
    {
        llm.max_tokens(n);
    }
    if let Some(v) = option.get("temperature")
        && let Ok(n) = v.parse::<f64>()
    {
        llm.temperature(n);
    }
    if let Some(v) = option.get("top_p")
        && let Ok(n) = v.parse::<f64>()
    {
        llm.top_p(n);
    }
    if let Some(v) = option.get("stop_sequences") {
        llm.stop_sequences(v.split(',').map(|s| s.to_string()).collect());
    }
    if let Some(v) = option.get("seed")
        && let Ok(n) = v.parse::<u64>()
    {
        llm.seed(n);
    }
    if let Some(v) = option.get("reasoning_effort") {
        if let Ok(level) = v.parse::<f64>() {
            llm.reasoning_level(level);
        } else if let Ok(eff) = v.parse::<ReasoningEffort>() {
            llm.reasoning_effort(eff);
        }
    }
    if let Some(v) = option.get("service_tier")
        && let Ok(n) = v.parse::<ServiceTier>()
    {
        llm.service_tier(n);
    }
    if let Some(v) = option.get("verbosity")
        && let Ok(n) = v.parse::<Verbosity>()
    {
        llm.verbosity(n);
    }

    // スキーマが frontmatter に指定されている場合、capability に応じて切り替える
    let has_schema = option.contains_key("schema");
    if let Some(schema_str) = option.get("schema") {
        match serde_json::from_str::<serde_json::Value>(schema_str) {
            Ok(schema_value) => {
                let name = option
                    .get("schema_name")
                    .map(|s| s.as_str())
                    .unwrap_or("output");
                if llm
                    .capabilities()
                    .contains(ModelCapability::STRUCTURED_OUTPUT)
                {
                    debug!("schema: using JsonSpec (structured output)");
                    llm.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                        name,
                        schema_value,
                    )));
                } else {
                    debug!("schema: falling back to prompt embedding");
                    llm.add(Content::Text(format!(
                        "回答は次のJSON schemaに厳密にしたがって生成せよ。\n\nJSON Schema:\n{}\n\n最終応答は、スキーマに適合するJSONのみを出力し、JSON以外の文字は一切含めないこと。",
                        schema_str
                    )));
                }
            }
            Err(e) => {
                return Err(LlmError::JsonParseError {
                    message: format!("Invalid schema in frontmatter: {}", e),
                });
            }
        }
    }

    let result = proc(llm).await;

    // schema が指定されていた場合、レスポンスが valid JSON であることを検証する。
    // そのままパースできない場合は、コードフェンスや前置きを剥がした `{...}` を
    // 切り出して再試行する(構造化出力非対応のモデルは、プロンプトで JSON を
    // 要求しても ```json フェンスを付けてくることがある)。
    if has_schema {
        return result.and_then(|s| {
            if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                return Ok(s);
            }
            match crate::text::extract_json(&s) {
                Some(json) if serde_json::from_str::<serde_json::Value>(json).is_ok() => {
                    debug!("schema: salvaged JSON from fenced/prefixed response");
                    Ok(json.to_string())
                }
                _ => Err(LlmError::JsonParseError {
                    message: format!(
                        "LLM response is not valid JSON: {}",
                        crate::text::shorten(&s, 200)
                    ),
                }),
            }
        });
    }
    result
}

#[cfg(test)]
mod tests_reasoning {
    use super::*;

    fn openai() -> Provider {
        Provider::OpenAI("gpt-5.3".to_string())
    }
    fn anthropic() -> Provider {
        Provider::Anthropic("claude-opus-4-8".to_string())
    }
    fn google() -> Provider {
        Provider::Google("gemini-3.1-pro-preview".to_string())
    }
    fn xai() -> Provider {
        Provider::XAi("grok-4.5".to_string())
    }
    fn lmstudio() -> Provider {
        Provider::LMStudio("model".to_string(), None)
    }

    #[test]
    fn test_zero_is_none() {
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 0.0),
            ReasoningEffort::None
        ));
        assert!(matches!(
            anthropic().map_reasoning("claude-opus-4-8", 0.0),
            ReasoningEffort::None
        ));
        assert!(matches!(
            google().map_reasoning("gemini-3.1-pro-preview", 0.0),
            ReasoningEffort::None
        ));
        assert!(matches!(
            lmstudio().map_reasoning("model", 0.0),
            ReasoningEffort::None
        ));
    }

    #[test]
    fn test_one_is_max_per_provider() {
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 1.0),
            ReasoningEffort::XHigh
        ));
        assert!(matches!(
            anthropic().map_reasoning("claude-opus-4-8", 1.0),
            ReasoningEffort::Max
        ));
        assert!(matches!(
            google().map_reasoning("gemini-3.1-pro-preview", 1.0),
            ReasoningEffort::High
        ));
        assert!(matches!(
            xai().map_reasoning("grok-4.5", 1.0),
            ReasoningEffort::High
        ));
    }

    #[test]
    fn test_openai_midpoints() {
        // ladder=[Low,Medium,High,XHigh], len=4
        // 0.25 -> ceil(1.0)=1 -> idx=0 -> Low
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 0.25),
            ReasoningEffort::Low
        ));
        // 0.5  -> ceil(2.0)=2 -> idx=1 -> Medium
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 0.5),
            ReasoningEffort::Medium
        ));
        // 0.75 -> ceil(3.0)=3 -> idx=2 -> High
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 0.75),
            ReasoningEffort::High
        ));
    }

    #[test]
    fn test_clamp_out_of_range() {
        // 1.0超 -> clamp to 1.0 -> 最上位
        assert!(matches!(
            openai().map_reasoning("gpt-5.3", 1.5),
            ReasoningEffort::XHigh
        ));
        // 負数 -> None
        assert!(matches!(
            anthropic().map_reasoning("claude-opus-4-8", -0.3),
            ReasoningEffort::None
        ));
    }

    #[test]
    fn test_lmstudio_always_none() {
        for v in [0.0, 0.5, 1.0, 2.0] {
            assert!(matches!(
                lmstudio().map_reasoning("model", v),
                ReasoningEffort::None
            ));
        }
    }

    #[test]
    fn test_xai_grok_4_20_reasoning_never_supports_effort() {
        // grok-4.20-0309-reasoning / -non-reasoning は reasoning 深度が
        // スナップショットに固定されており、reasoning_effort 自体が非対応。
        // 送ると xAI が 400 "does not support parameter reasoningEffort" を返す
        // (このバグの元になった実際のエラー)。
        for v in [0.0, 0.5, 1.0] {
            assert!(matches!(
                xai().map_reasoning("grok-4.20-0309-reasoning", v),
                ReasoningEffort::None
            ));
            assert!(matches!(
                xai().map_reasoning("grok-4.20-0309-non-reasoning", v),
                ReasoningEffort::None
            ));
        }
    }

    #[test]
    fn test_xai_grok_4_5_supports_effort() {
        assert!(matches!(
            xai().map_reasoning("grok-4.5", 1.0),
            ReasoningEffort::High
        ));
    }

    #[test]
    fn test_xai_grok_4_20_multi_agent_is_not_mistaken_for_grok_4_20() {
        // "grok-4.20-multi-agent" は "grok-4.20" のプレフィックスにもマッチしうるため、
        // multi-agent の判定を先に行う必要がある。ここでは reasoning_effort が
        // 「エージェント数」に転用され、1.0 で最上位(XHigh)に到達することを確認する。
        assert!(matches!(
            xai().map_reasoning("grok-4.20-multi-agent-0309", 1.0),
            ReasoningEffort::XHigh
        ));
        assert!(matches!(
            xai().map_reasoning("grok-4.20-multi-agent-0309", 0.0),
            ReasoningEffort::None
        ));
    }

    #[test]
    fn test_google_gemini_3_pro_preview_bare_has_no_medium() {
        // 実際に確認された回帰: gemini-3-pro-preview(`.1` 無し)に thinkingLevel=MEDIUM
        // を送ると 400 "Thinking level MEDIUM is not supported for this model." になる
        // (xAI の reasoningEffort 400 と同じ「プロバイダ単位で決め打ちしていた」型のバグ)。
        // 0.5 は 3段ラダーなら Medium だが、2段ラダー([Low, High])では Low 側に丸まる
        // (ceil(0.5*2)=1 -> idx=0)。0.5 超で初めて High に切り上がる。
        assert!(matches!(
            google().map_reasoning("gemini-3-pro-preview", 0.5),
            ReasoningEffort::Low
        ));
        assert!(matches!(
            google().map_reasoning("gemini-3-pro-preview", 0.51),
            ReasoningEffort::High
        ));
        assert!(matches!(
            google().map_reasoning("gemini-3-pro-preview", 1.0),
            ReasoningEffort::High
        ));
        assert!(matches!(
            google().map_reasoning("gemini-3-pro-preview", 0.0),
            ReasoningEffort::None
        ));
    }

    #[test]
    fn test_google_gemini_3_1_pro_preview_still_has_medium() {
        // "gemini-3-pro-preview" への前方一致が誤って "gemini-3.1-pro-preview" にまで
        // 波及していないこと("." の位置が違うので前方一致しないはず)。
        assert!(matches!(
            google().map_reasoning("gemini-3.1-pro-preview", 0.5),
            ReasoningEffort::Medium
        ));
    }
}

#[cfg(test)]
mod tests_capabilities {
    use super::*;

    #[test]
    fn test_openai_google_anthropic_always_full() {
        // OpenAI/Google/Anthropic はモデル名に依らず全capability対応
        // (Anthropic は genai の adapter が output_config.format 経由で対応済み)
        let openai = Provider::OpenAI("gpt-5.3".to_string());
        let google = Provider::Google("gemini-3.1-pro-preview".to_string());
        let anthropic = Provider::Anthropic("claude-4.6-sonnet".to_string());
        let expected = ModelCapability::STRUCTURED_OUTPUT
            | ModelCapability::TOOL_CALLING
            | ModelCapability::REASONING_EFFORT
            | ModelCapability::STOP_SEQUENCES;
        assert_eq!(openai.default_capabilities("gpt-5.3"), expected);
        assert_eq!(
            google.default_capabilities("gemini-3.1-pro-preview"),
            expected
        );
        assert_eq!(
            anthropic.default_capabilities("claude-4.6-sonnet"),
            expected
        );
    }

    #[test]
    fn test_xai_reasoning_models_get_reasoning_effort() {
        let xai = Provider::XAi("grok-4.5".to_string());
        let with_reasoning = ModelCapability::STRUCTURED_OUTPUT
            | ModelCapability::TOOL_CALLING
            | ModelCapability::REASONING_EFFORT;
        assert_eq!(xai.default_capabilities("grok-4.5"), with_reasoning);
        assert_eq!(xai.default_capabilities("grok-4.3"), with_reasoning);
        assert_eq!(
            xai.default_capabilities("grok-4.20-multi-agent-0309"),
            with_reasoning
        );
    }

    #[test]
    fn test_xai_grok_4_20_snapshot_has_no_reasoning_effort() {
        // 実際の回帰: grok-4.20-0309-reasoning に reasoning_effort を送ると
        // xAI が 400 "does not support parameter reasoningEffort" を返す。
        let xai = Provider::XAi("grok-4.20-0309-reasoning".to_string());
        let without_reasoning = ModelCapability::STRUCTURED_OUTPUT | ModelCapability::TOOL_CALLING;
        assert_eq!(
            xai.default_capabilities("grok-4.20-0309-reasoning"),
            without_reasoning
        );
        assert_eq!(
            xai.default_capabilities("grok-4.20-0309-non-reasoning"),
            without_reasoning
        );
    }

    #[test]
    fn test_xai_unknown_model_defaults_conservative() {
        // 静的表に無い未知の Grok モデルは reasoning_effort を送らない
        // (400 の場合は Step 5 の自己修復リトライが吸収する)。
        let xai = Provider::XAi("grok-4.5".to_string());
        assert_eq!(
            xai.default_capabilities("grok-9-mystery"),
            ModelCapability::STRUCTURED_OUTPUT | ModelCapability::TOOL_CALLING
        );
    }

    #[test]
    fn test_lmstudio_always_empty_regardless_of_model() {
        let lmstudio = Provider::LMStudio("model".to_string(), None);
        assert_eq!(
            lmstudio.default_capabilities("qwen3.5-2b-instruct"),
            ModelCapability::empty()
        );
    }

    #[test]
    fn test_cloudflare_tool_calling_model_detected() {
        let cf = Provider::Cloudflare("@cf/meta/llama-3.1-8b-instruct".to_string());
        assert_eq!(
            cf.default_capabilities("@cf/meta/llama-3.1-8b-instruct"),
            ModelCapability::TOOL_CALLING
        );
    }

    #[test]
    fn test_cloudflare_unknown_model_defaults_empty() {
        let cf = Provider::Cloudflare("@cf/some/unknown-model".to_string());
        assert_eq!(
            cf.default_capabilities("@cf/some/unknown-model"),
            ModelCapability::empty()
        );
    }

    #[test]
    fn test_cloudflare_uses_effective_model_not_provider_default() {
        // Provider が保持する既定モデルではなく、渡された実効モデル名で判定されること
        let cf = Provider::Cloudflare("@cf/meta/llama-3.1-8b-instruct".to_string());
        assert_eq!(
            cf.default_capabilities("@cf/qwen/qwen1.5-14b-chat"),
            ModelCapability::TOOL_CALLING // "qwen" キーワードにマッチ
        );
    }
}

#[cfg(test)]
mod tests_unsupported_param_retry {
    use super::*;

    #[test]
    fn test_extracts_reasoning_effort_from_actual_xai_error() {
        // このバグ報告の実際のエラー本文(そのまま)
        let body = "Model grok-4.20-0309-reasoning does not support parameter reasoningEffort.";
        assert_eq!(
            unsupported_param_from_error_body(body).as_deref(),
            Some("reasoning_effort")
        );
    }

    #[test]
    fn test_extracts_from_quoted_json_style_body() {
        let body = r#"{"code":"Invalid request","error":"does not support parameter \"stop\""}"#;
        assert_eq!(
            unsupported_param_from_error_body(body).as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn test_unrelated_body_returns_none() {
        let body = "Internal server error";
        assert_eq!(unsupported_param_from_error_body(body), None);
    }

    #[test]
    fn test_camel_to_snake() {
        assert_eq!(camel_to_snake("reasoningEffort"), "reasoning_effort");
        assert_eq!(camel_to_snake("stop"), "stop");
        assert_eq!(camel_to_snake("stopSequences"), "stop_sequences");
    }

    #[test]
    fn test_strip_unsupported_option_reasoning_effort() {
        let mut options = ChatOptions::default().with_reasoning_effort(ReasoningEffort::High);
        assert!(strip_unsupported_option(&mut options, "reasoning_effort"));
        assert!(options.reasoning_effort.is_none());
        // 既に外れている場合は false (無限リトライを避けるためのガード)
        assert!(!strip_unsupported_option(&mut options, "reasoning_effort"));
    }

    #[test]
    fn test_strip_unsupported_option_stop_sequences() {
        let mut options = ChatOptions::default().with_stop_sequences(vec!["STOP".to_string()]);
        assert!(strip_unsupported_option(&mut options, "stop"));
        assert!(options.stop_sequences.is_empty());
        assert!(!strip_unsupported_option(&mut options, "stop_sequences"));
    }

    #[test]
    fn test_strip_unsupported_option_unknown_param_is_noop() {
        let mut options = ChatOptions::default();
        assert!(!strip_unsupported_option(&mut options, "top_logprobs"));
    }
}

#[cfg(test)]
mod tests_tool_round {
    use super::*;
    use genai::chat::ChatRole;

    /// テスト用の最小 LlmTool。args の "msg" をそのまま返す。
    #[derive(Debug)]
    struct EchoTool;

    #[async_trait]
    impl LlmTool for EchoTool {
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo tool for tests"
        }
        async fn invoke(&self, args: &Map<String, Value>) -> Result<String, LlmError> {
            Ok(format!(
                "echo:{}",
                args.get("msg").and_then(|v| v.as_str()).unwrap_or("")
            ))
        }
    }

    fn client_with_echo_tool() -> Box<dyn LlmInterface> {
        let mut cl =
            LlmClientBuilder::from_value(&serde_json::json!({"provider": "lmstudio"})).build();
        cl.add_tool(Box::new(EchoTool));
        cl
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: serde_json::json!({"msg": "hello"}),
            thought_signatures: None,
        }
    }

    #[tokio::test]
    async fn test_respond_tool_returns_assistant_then_tool() {
        let cl = client_with_echo_tool();
        let follow = cl.respond_tool(&[tool_call("c1", "echo")]).await.unwrap();

        // Assistant(functionCall) → Tool(functionResponse) の順で2メッセージ。
        // From<Vec<ToolResponse>> は Tool ロールになる(Gemini 出力は User と同一)。
        assert_eq!(follow.messages.len(), 2);
        assert!(matches!(follow.messages[0].role, ChatRole::Assistant));
        assert!(matches!(follow.messages[1].role, ChatRole::Tool));
        assert_eq!(follow.messages[0].content.tool_calls().len(), 1);
        // ツールの実行結果が functionResponse に入っていること
        let serialized = serde_json::to_string(&follow.messages[1]).unwrap();
        assert!(serialized.contains("echo:hello"), "{serialized}");
    }

    #[tokio::test]
    async fn test_respond_tool_preserves_thought_signature() {
        // モデルが functionCall に付けて返した thought_signature を、送信用の
        // Assistant メッセージの先頭 ThoughtSignature パートへ hoist すること
        // (Gemini 3 はこの署名を verbatim で戻さないと 400 を返す)。
        let cl = client_with_echo_tool();
        let mut tc = tool_call("c1", "echo");
        tc.thought_signatures = Some(vec!["sig-abc".to_string()]);

        let follow = cl.respond_tool(&[tc]).await.unwrap();

        let assistant = &follow.messages[0].content;
        assert!(
            assistant.contains_thought_signature(),
            "Assistant メッセージに ThoughtSignature パートが含まれること"
        );
        assert_eq!(
            assistant.first_thought_signature(),
            Some("sig-abc"),
            "モデルの署名が保持されること"
        );
        // 署名を hoist しても functionCall 自体は保持される
        assert_eq!(assistant.tool_calls().len(), 1);
    }

    #[tokio::test]
    async fn test_respond_tool_unknown_tool_gets_error_response() {
        let cl = client_with_echo_tool();
        let follow = cl
            .respond_tool(&[tool_call("c1", "echo"), tool_call("c2", "nonexistent")])
            .await
            .unwrap();

        // 未知ツールも functionCall 履歴から除外せず、エラー応答を返す
        // (除外すると functionCall と functionResponse の件数が食い違う)
        assert_eq!(follow.messages[0].content.tool_calls().len(), 2);
        let serialized = serde_json::to_string(&follow.messages[1]).unwrap();
        assert!(
            serialized.contains("Error: unknown tool nonexistent"),
            "{serialized}"
        );
        assert!(serialized.contains("echo:hello"), "{serialized}");
    }

    #[test]
    fn test_tool_round_appends_to_history() {
        // with_model のツールラウンド処理と同じマージ: 既存履歴へ追記し、
        // system / user プロンプト / tools 宣言が保持されること
        // (Gemini は functionCall ターンが user ターンの直後に無いと 400 を返す)
        let mut chat_req = ChatRequest::from_system("sys")
            .append_message(ChatMessage::user("prompt"))
            .with_tools(vec![Tool::new("echo")]);

        let follow = ChatRequest::from_messages(vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: genai::chat::MessageContent::from_tool_calls(vec![tool_call(
                    "c1", "echo",
                )]),
                options: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: genai::chat::MessageContent::from_parts(vec![
                    genai::chat::ContentPart::ToolResponse(ToolResponse::new(
                        "c1".to_string(),
                        "echo:hello".to_string(),
                    )),
                ]),
                options: None,
            },
        ]);

        chat_req.messages.extend(follow.messages);

        assert_eq!(chat_req.system.as_deref(), Some("sys"));
        assert!(chat_req.tools.is_some(), "tools 宣言が保持されること");
        assert_eq!(chat_req.messages.len(), 3);
        assert!(matches!(chat_req.messages[0].role, ChatRole::User));
        assert!(matches!(chat_req.messages[1].role, ChatRole::Assistant));
        assert!(matches!(chat_req.messages[2].role, ChatRole::User));
        assert_eq!(chat_req.messages[1].content.tool_calls().len(), 1);
    }

    /// 送信に失敗しても一時プロンプトを次リクエストへ持ち越さないこと。
    /// (回帰: 補完連打時にプロンプトが前回分ごと累積し 3800 文字超まで膨張していた。
    ///  原因は with_model の成功パスにしか self.prompts.clear() が無かったこと)
    #[tokio::test]
    async fn test_failed_chat_does_not_leak_prompts() {
        // ポート1は予約済みで待ち受けが存在しない → 接続拒否で即エラーになる
        let mut cl = LlmClientBuilder::from_value(
            &serde_json::json!({"provider": "lmstudio", "url": "http://127.0.0.1:1/"}),
        )
        .build();

        cl.add(Content::Text("テキスト：トン0トン".into()));
        assert!(cl.chat().await.is_err(), "接続できずエラーになる前提");

        assert!(
            !cl.build_content().contains("トン0トン"),
            "失敗したリクエストのプロンプトが残っている: {}",
            cl.build_content()
        );
    }
}
