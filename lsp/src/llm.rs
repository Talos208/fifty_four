use farmhash;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ClientBuilder, ModelIden, ModelName, ServiceTarget};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display, LowerHex};
use std::sync::{Arc, Mutex};
use tower_lsp::async_trait;
use tower_lsp::lsp_types::CompletionOptionsCompletionItem;

/// LLMプロバイダを表すenum
#[derive(Debug, Clone)]
pub enum Provider {
    Google,
    OpenAI,
    Anthropic,
    XAi,
    LMStudio,

    Undefined,
}

impl Provider {
    /// 文字列からProviderを生成する
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "google" => Ok(Provider::Google),
            "openai" => Ok(Provider::OpenAI),
            "anthropic" => Ok(Provider::Anthropic),
            "xai" => Ok(Provider::XAi),
            "lmstudio" => Ok(Provider::LMStudio),
            _ => Err(format!("Unsupported provider: {}", s)),
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

#[async_trait]
pub trait LlmClient: Send + Sync + std::fmt::Debug {
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
        write!(f, "LlmClient")
    }

    fn build_content(&self) -> String;

    fn get_model(&self) -> &str;
}

#[async_trait]
pub trait Caching: std::fmt::Debug {
    /// キャッシュを初期化（クリア）する
    fn clear(&mut self);

    /// プロンプトを永続的なキャッシュに追加する
    ///
    /// このプロンプトは、`chat`が呼び出されるたびに再利用されます。
    fn cache(&mut self, prompt: Content) -> Result<String, genai::Error>;

    fn fetch(&self, hash: &String) -> Option<&Content>;

    fn fetch_all(&self) -> Vec<Content>;

    fn delete(&mut self, hash: String);
}

/// `LlmClient`トレイトの汎用的な実装
///
/// `genai::Client`を内部的に利用し、特定のLLMプロバイダに依存しない形で
/// チャット機能を提供します。プロンプトの管理には`Cache`トレイトを利用します。
#[derive(Debug, Clone)]
pub struct GenericLlmClient {
    /// `genai`ライブラリのコアクライアント
    inner_client: Client,
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
                if let Content::Text(s) = c {
                    Some(ChatMessage::system(s))
                } else {
                    None
                }
            }))
            .append_messages(self.prompts.iter().filter_map(|c| {
                if let Content::Text(s) = c {
                    Some(ChatMessage::user(s))
                } else {
                    None
                }
            }));
        let chat_opt = Some(&ChatOptions::default());
        let response = self
            .inner_client
            .exec_chat(model, chat_req, chat_opt)
            .await?;
        let content = response.content.texts().join("\n");

        self.prompts.clear(); // promptはクリア、キャッシュは保存

        Ok(content)
    }

    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GenericLlmClient :{}", self.model)
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
            .join("\n")
    }

    fn get_model(&self) -> &str {
        &self.model
    }
}

impl Caching for GenericLlmClient {
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

    fn fetch(&self, hash: &String) -> Option<&Content> {
        self.cache.get(hash)
    }

    fn fetch_all(&self) -> Vec<Content> {
        self.cache.values().map(|c| c.clone()).collect()
    }

    fn clear(&mut self) {
        self.cache.clear();
    }

    fn delete(&mut self, hash: String) {
        self.cache.remove(&hash);
    }
}

impl GenericLlmClient {
    pub fn new(provider: Provider, model: &str, url: Option<&str>) -> GenericLlmClient {
        match provider {
            Provider::Google | Provider::OpenAI | Provider::Anthropic | Provider::XAi => {
                GenericLlmClient {
                    inner_client: GenericLlmClient::builder(provider, model, url).build(),
                    model: model.to_string(),
                    cache: HashMap::new(),
                    prompts: vec![],
                }
            }
            // TODO Resultにしてエラー返すべきだろう
            _ => GenericLlmClient {
                inner_client: GenericLlmClient::builder(provider, model, url).build(),
                model: model.to_string(),
                cache: HashMap::new(),
                prompts: vec![],
            },
        }
    }

    pub fn from_name(model_name: &str) -> GenericLlmClient {
        let (mut prov, mut model) = model_name.split_once('/').unwrap();

        if model.is_empty() {
            model = prov;
            if let Some((token, _)) = model.split_once('-') {
                prov = match token {
                    "gemini" => "google",
                    "gpt" => "openai",
                    "claude" => "anthropic",
                    "grok" => "xai",
                    "lmstudio" => "lmstudio",
                    _ => "Unknown",
                };
            }
        }

        GenericLlmClient {
            inner_client: GenericLlmClient::builder(Provider::from_str(prov).unwrap(), model, None)
                .build(),
            model: String::from(model),
            cache: HashMap::new(),
            prompts: vec![],
        }
    }

    /// LLMビルダーを構築する
    fn builder(_provider: Provider, model_name: &str, url: Option<&str>) -> ClientBuilder {
        let endpoint = url.map(Endpoint::from_owned);

        let (auth, kind) = match _provider {
            Provider::Google => (
                AuthData::FromEnv("GEMINI_API_KEY".to_string()),
                AdapterKind::Gemini,
            ),
            Provider::OpenAI => (
                AuthData::FromEnv("OPENAI_API_KEY".to_string()),
                AdapterKind::OpenAI,
            ),
            Provider::Anthropic => (
                AuthData::FromEnv("ANTHROPIC_API_KEY".to_string()),
                AdapterKind::Anthropic,
            ),
            Provider::XAi => (
                AuthData::FromEnv("XAI_API_KEY".to_string()),
                AdapterKind::Xai,
            ),
            Provider::LMStudio => (
                AuthData::Key("".to_string()), // No authentication required for local LMStudio
                AdapterKind::Anthropic,
            ),
            _ => panic!("Unsupported provider"),
        };
        let model = ModelIden {
            adapter_kind: kind,
            model_name: ModelName::from(model_name),
        };

        let proc = ServiceTargetResolver::from_resolver_fn(
            |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                Ok(ServiceTarget {
                    endpoint: endpoint.unwrap_or(target.endpoint),
                    auth,
                    model,
                })
            },
        );

        genai::Client::builder().with_service_target_resolver(proc)
    }
}
