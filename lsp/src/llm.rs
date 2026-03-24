use async_trait::async_trait;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ReasoningEffort, ServiceTier,
    Verbosity,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ModelName, ServiceTarget};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};

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

#[async_trait]
#[allow(unused)]
pub trait LlmClient: Send + Sync + std::fmt::Debug {
    //+ MaybeUninit {
    /// LLMとチャットセッションを実行する
    ///
    /// 現在のプロンプトを基にLLMにリクエストを送信し、応答を文字列として返します。
    async fn chat(&mut self) -> Result<String, Box<dyn Error>>;

    async fn with_model(&mut self, model: &str) -> Result<String, Box<dyn Error>>;

    /// 一時的なプロンプトを現在のセッションに追加する
    ///
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
    fn cache(&mut self, prompt: Content) -> Result<String, genai::Error>;

    fn fetch(&self, hash: &str) -> Option<&Content>;

    fn fetch_all(&self) -> Vec<Content>;

    fn remove(&mut self, hash: String);

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
}

#[derive(Debug, Clone)]
pub struct LlmClientBuilder {
    provider: Provider,
    model: Option<String>,
    url: Option<String>,
}

impl LlmClientBuilder {
    /// LLMビルダーを構築する
    #[allow(unused)]
    fn new(_provider: Provider) -> LlmClientBuilder {
        LlmClientBuilder {
            provider: _provider.clone(),
            model: None,
            url: None,
        }
    }

    #[allow(unused)]
    pub fn from_name(name: &str) -> Self {
        LlmClientBuilder {
            provider: Provider::from_name(name).unwrap(),
            model: None,
            url: None,
        }
    }

    pub fn from_value(value: &serde_json::Value) -> Self {
        LlmClientBuilder {
            provider: Provider::from_str(value["provider"].as_str().unwrap()).unwrap(),
            model: value.get("model").map(|v| v.as_str().unwrap().to_string()),
            url: value.get("url").map(|v| v.as_str().unwrap().to_string()),
        }
    }

    pub fn model(&mut self, model: &str) -> Self {
        Self {
            model: Some(model.to_string()),
            provider: self.provider.clone(),
            url: self.url.clone(),
        }
    }

    pub fn url(&mut self, url: &str) -> Self {
        Self {
            url: Some(url.to_string()),
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }

    pub fn build(&self) -> Box<dyn LlmClient> {
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
                debug!("{:?}, {:?}, {:?}", e, auth, model_iden);
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

        Box::new(GenericLlmClient {
            inner_client,
            model: mdl_name.clone(),
            cache: HashMap::new(),
            prompts: vec![],
            options: ChatOptions::default(),
        })
    }
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
}

#[async_trait]
impl LlmClient for GenericLlmClient {
    async fn chat(&mut self) -> Result<String, Box<dyn Error>> {
        let model = self.model.clone();
        self.with_model(model.as_str()).await
    }

    async fn with_model(&mut self, model: &str) -> Result<String, Box<dyn Error>> {
        // TODO AGENTS.mdをfrom_system()で投入
        let chat_req = ChatRequest::from_messages(vec![])
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
            }));

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
                } => {
                    error!("{:?}", e)
                }
                genai::Error::ChatResponseGeneration {
                    model_iden: _,
                    request_payload: _,
                    response_body: _,
                    cause: ref c,
                } => {
                    error!("{:?}", c.clone())
                }
                _ => {}
            };
            return Err(Box::new(err));
        };
        let content = response.content.texts().join("");

        self.prompts.clear(); // promptはクリア、キャッシュは保存
        self.options = ChatOptions::default();

        Ok(content)
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

    fn cache(&mut self, prompt: Content) -> Result<String, genai::Error> {
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
                    Err(genai::Error::Internal("cache not found".to_string()))
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
