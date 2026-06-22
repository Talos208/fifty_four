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

    Undefined,
}

impl Provider {
    /// 文字列からProviderを生成する
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "google" => Ok(Provider::Google("gemini-3.1-pro-preview".to_string())),
            "openai" => Ok(Provider::OpenAI("gpt-5.3".to_string())),
            "anthropic" => Ok(Provider::Anthropic("claude-4.6-sonnet".to_string())),
            "xai" => Ok(Provider::XAi("grok-4.1".to_string())),
            "lmstudio" => Ok(Provider::LMStudio(
                "qwen3.5-2b".to_string(),
                Some("http://localhost:1234/v1/".to_string()),
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
            _ => Err(format!("Unsupported provider: {}", name)),
        }
    }

    pub fn default_capabilities(&self) -> ModelCapability {
        match self {
            Provider::Google(_) | Provider::OpenAI(_) => {
                ModelCapability::STRUCTURED_OUTPUT | ModelCapability::TOOL_CALLING
            }
            Provider::Anthropic(_) | Provider::XAi(_) => ModelCapability::TOOL_CALLING,
            Provider::LMStudio(..) | Provider::Undefined => ModelCapability::empty(),
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
            _ => Provider::Undefined,
        }
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
pub trait LlmClient: Send + Sync + std::fmt::Debug {
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

    pub fn build(self) -> Box<dyn LlmClient> {
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
            .unwrap_or_else(|| self.provider.default_capabilities());

        Box::new(GenericLlmClient {
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

/// `LlmClient`トレイトの汎用的な実装
///
/// `genai::Client`を内部的に利用し、特定のLLMプロバイダに依存しない形で
/// チャット機能を提供します。プロンプトの管理には`Cache`トレイトを利用します。
#[derive(Debug)]
pub struct GenericLlmClient {
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
impl LlmClient for GenericLlmClient {
    async fn chat(&mut self) -> Result<String, LlmError> {
        let model = self.model.clone();
        self.with_model(model.as_str()).await
    }

    fn capabilities(&self) -> ModelCapability {
        self.capabilities
    }

    fn add_tool(&mut self, tool: Box<dyn LlmTool>) {
        self.tools.insert(tool.as_ref().name().to_string(), tool);
        debug!("Tool added to backend {:?}", self.tools);
    }

    async fn with_model(&mut self, model: &str) -> Result<String, LlmError> {
        // TODO AGENTS.mdをfrom_system()で投入
        let mut chat_req = ChatRequest::from_system(&self.sys_prompt)
            .append_messages(self.fetch_all().iter().filter_map(|c| {
                Some(ChatMessage::system(match c {
                    Content::Text(s) => s,
                    Content::CacheEntry(h) => self.fetch(h)?.as_ref(),
                }))
            }))
            .append_messages(self.prompts.iter().filter_map(|c| {
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

        let content = loop {
            let res = self
                .inner_client
                .exec_chat(model, chat_req, Some(&self.options))
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

            self.prompts.clear(); // promptはクリア、キャッシュは保存
            self.options = ChatOptions::default();

            if let Some(reason) = response.reasoning_content.as_ref() {
                debug!("reasoning: {}", reason)
            }

            if response.tool_calls().is_empty() {
                break response.content.texts().join("");
            }

            // Toolcallが無くなるまでループ
            let tc = response.into_tool_calls();
            match self.respond_tool(tc.as_slice()).await {
                Ok(ret) => chat_req = ret,
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
            let Some(t) = self.tools.get(&tc.fn_name) else {
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
            result_tc.push(tc.clone());
        }

        let chat_req = ChatRequest::from_messages(vec![])
            .append_message(ChatMessage {
                role: genai::chat::ChatRole::Assistant,
                content: genai::chat::MessageContent::from_tool_calls(result_tc),
                options: None,
            })
            .append_message(ChatMessage {
                role: genai::chat::ChatRole::User,
                content: genai::chat::MessageContent::from_parts(
                    result_tr
                        .into_iter()
                        .map(genai::chat::ContentPart::ToolResponse)
                        .collect::<Vec<_>>(),
                ),
                options: None,
            });
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
        self.options = std::mem::take(&mut self.options).with_stop_sequences(seqs);
    }

    fn seed(&mut self, v: u64) {
        self.options = std::mem::take(&mut self.options).with_seed(v);
    }

    fn reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.options = std::mem::take(&mut self.options).with_reasoning_effort(effort);
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
