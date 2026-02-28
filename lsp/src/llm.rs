use genai::adapter::AdapterKind;
use genai::chat::{ChatOptions, ChatRequest};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ClientBuilder, ModelIden, ModelName, ServiceTarget};
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};
use tower_lsp::async_trait;

/// LLMプロバイダを表すenum
#[derive(Debug, Clone)]
pub enum Provider {
    Google,
    OpenAI,
    Anthropic,
    XAI,
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
            "xai" => Ok(Provider::XAI),
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
#[async_trait]
pub trait LlmClient: Send + Sync + std::fmt::Debug + Sized + Clone {
    /// LLMとチャットセッションを実行する
    ///
    /// 現在のプロンプトを基にLLMにリクエストを送信し、応答を文字列として返します。
    async fn chat(&mut self) -> Result<String, Box<dyn Error>>;

    async fn with_model(&mut self, model: &str) -> Result<String, Box<dyn Error>>;

    /// プロンプトを永続的なキャッシュに追加する
    ///
    /// このプロンプトは、`chat`が呼び出されるたびに再利用されます。
    fn cache(&mut self, prompt: String);

    /// 一時的なプロンプトを現在のセッションに追加する
    ///
    /// このプロンプトは、次回の`chat`呼び出しでのみ使用され、その後クリアされます。
    fn add(&mut self, prompt: String);

    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LlmClient")
    }
}

/// プロンプトをキャッシュするためのストレージ機能を定義するトレイト
///
/// `init`, `set`, `all`の3つのメソッドを通じて、キャッシュの初期化、
/// 値の保存、そしてすべてのキャッシュ内容の取得を行います。
trait Cache: Send + Sync + std::fmt::Debug {
    /// キャッシュを初期化（クリア）する
    fn init(&self);

    /// 新しい値をキャッシュに保存する
    fn set(&self, value: String);

    /// キャッシュされているすべての値を取得する
    fn all(&self) -> Vec<String>;
}

/// `Cache`トレイトのインメモリ実装
///
/// `Arc<Mutex<Vec<String>>>`を使用して、スレッドセーフなオンメモリキャッシュを提供します。
/// アプリケーションの実行中、プロンプトを保持するために使われます。
#[derive(Clone, Debug)]
struct PromptCache {
    store: Arc<Mutex<Vec<String>>>,
}

impl PromptCache {
    /// 新しい`PromptCache`インスタンスを作成する
    fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Cache for PromptCache {
    fn init(&self) {
        self.store.lock().unwrap().clear();
    }

    fn set(&self, value: String) {
        let mut store = self.store.lock().unwrap();
        store.push(value);
    }

    fn all(&self) -> Vec<String> {
        self.store.lock().unwrap().clone()
    }
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
    // cache: Box<dyn Cache>,
    /// 現在のチャットセッションでのみ使用される一時的なプロンプト
    prompts: Vec<String>,
}

#[tower_lsp::async_trait]
impl LlmClient for GenericLlmClient {
    async fn chat(&mut self) -> Result<String, Box<dyn Error>> {
        let model = self.model.clone();
        self.with_model(model.as_str()).await
    }

    async fn with_model(&mut self, model: &str) -> Result<String, Box<dyn Error>> {
        // let mut plots = self.cache.all();
        let mut plots = vec![];
        plots.extend_from_slice(&self.prompts);

        // TODO AGENTS.mdをfrom_system()で投入
        let chat_req = ChatRequest::from_user(plots.join("\n"));
        let chat_opt = Some(&ChatOptions::default());
        let response = self
            .inner_client
            .exec_chat(model, chat_req, chat_opt)
            .await?;
        let content = response.content.texts().join("\n");

        self.prompts.clear(); // promptはクリア、キャッシュは保存

        Ok(content)
    }

    fn cache(&mut self, prompt: String) {
        self.prompts.push(prompt); // TODO: Implement caching logic
    }

    fn add(&mut self, prompt: String) {
        self.prompts.push(prompt);
    }
}

impl GenericLlmClient {
    pub fn new(provider: Provider, model: &str, url: Option<&str>) -> GenericLlmClient {
        // 暫定。Cacheの実装もプロバイダで変えなきゃダメそうな予感
        let _cache: Box<dyn Cache> = Box::new(PromptCache::new());

        match provider {
            Provider::Google | Provider::OpenAI | Provider::Anthropic | Provider::XAI => {
                GenericLlmClient {
                    inner_client: GenericLlmClient::builder(provider, model, url).build(),
                    model: model.to_string(),
                    // cache,
                    prompts: vec![],
                }
            }
            // TODO Resultにしてエラー返すべきだろう
            _ => GenericLlmClient {
                inner_client: GenericLlmClient::builder(provider, model, url).build(),
                model: model.to_string(),
                // cache,
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

        // 暫定。Cacheの実装もプロバイダで変えなきゃダメそうな予感
        let _cache: Box<dyn Cache> = Box::new(PromptCache::new());

        GenericLlmClient {
            inner_client: GenericLlmClient::builder(Provider::from_str(prov).unwrap(), model, None)
                .build(),
            model: String::from(model),
            // cache,
            prompts: vec![],
        }
    }

    /// LLMビルダーを構築する
    fn builder(_provider: Provider, model_name: &str, url: Option<&str>) -> ClientBuilder {
        let endpoint = url.map(Endpoint::from_owned);

        let (auth, kind) = match _provider {
            Provider::Google => (
                AuthData::FromEnv("GOOGLE_API_KEY".to_string()),
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
            Provider::XAI => (
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
