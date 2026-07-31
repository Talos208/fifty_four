//! ACP (Agent Client Protocol) エージェント。
//!
//! `fifty_four_lsp --acp` で起動したときのモード。Zed の Agent Panel から
//! stdio 越しに接続され、作者とのチャットを LLM へ中継する。
//!
//! 目的はチャット UI を足すことそのものではなく、**作者が「いま何を書こうと
//! しているか」を LSP の短文生成へ渡す**こと。1ターンごとに会話を要約し、
//! [`crate::chat_context`] のファイルへ書き出す。LSP 側は補完・code action の
//! プロンプト組み立て時にそれを読んで `{{CHAT}}` へ埋め込む。
//!
//! # Zed 側の設定
//!
//! Zed の拡張 API には ACP エージェントを登録する口が無い(language server /
//! MCP context server / DAP のみ)。したがってユーザの `settings.json` に
//! 手で書いてもらう必要がある。詳細は `docs/acp-agent.md` を参照。
//!
//! # 実装上の制約
//!
//! [`LlmInterface`] はマルチターン会話を持たない([`crate::llm::LlmClient::with_model`]
//! が毎回 `ChatRequest` を組み直す)。そのため会話履歴はテンプレートへ文字列として
//! レンダリングして渡している。また `chat()` は完成した文字列を返すだけで
//! ストリーミングを公開していないため、1ターンにつき1チャンクを送る。

use crate::llm::{Content, LlmError, LlmInterface};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, Stdio};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 応答生成のプロンプトへ載せる過去ターン数の上限。
///
/// `LlmInterface` に会話履歴が無く毎回テンプレートへ焼き込む方式なので、
/// 際限なく伸ばすとリクエストが肥大する。
const MAX_HISTORY_TURNS: usize = 12;

/// 要約に渡す過去ターン数の上限。
///
/// 要約が拾うべきなのは「いま書こうとしている場面」なので、応答生成より狭くてよい。
const MAX_DIGEST_TURNS: usize = 8;

/// LLM クライアントの置き場所。`Backend` と同じ形にして
/// [`crate::llm::use_llm_with_option`] をそのまま使えるようにする。
type LlmSlot = Arc<tokio::sync::Mutex<Option<Box<dyn LlmInterface>>>>;

/// 会話の話者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    /// 作者(Zed の Agent Panel で入力した人)
    Author,
    /// このエージェント
    Agent,
}

impl Speaker {
    /// プロンプトへ書き出すときのラベル。
    fn label(self) -> &'static str {
        match self {
            Speaker::Author => "作者",
            Speaker::Agent => "アシスタント",
        }
    }
}

/// 会話の1発話。
#[derive(Debug, Clone)]
pub(crate) struct ChatTurn {
    pub(crate) speaker: Speaker,
    pub(crate) text: String,
}

/// セッションごとの状態。
#[derive(Debug)]
struct Session {
    /// `session/new` で渡されたワークスペースルート。要約の書き出し先の決定に使う。
    root: PathBuf,
    turns: Vec<ChatTurn>,
}

/// ハンドラ間で共有する状態。
struct AgentState {
    sessions: tokio::sync::Mutex<HashMap<SessionId, Session>>,
    /// チャット応答の生成用(`llm.ondemand`)
    llm: LlmSlot,
    /// 要約の生成用(`llm.deferred`)。応答生成と別スロットにして、
    /// 要約が次のターンの応答をブロックしないようにする。
    digest_llm: LlmSlot,
    next_session_no: std::sync::atomic::AtomicU64,
}

impl AgentState {
    /// 新しいセッションIDを採番する。
    ///
    /// 複数の ACP プロセスが同時に動いてもぶつからないよう PID を含める。
    fn new_session_id(&self) -> SessionId {
        let n = self
            .next_session_no
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SessionId::new(format!("ff-{}-{}", std::process::id(), n))
    }
}

/// ACP エージェントとして stdio で待ち受ける。
///
/// LLM 設定が読めない場合はここで `Err` を返す(呼び出し元が終了コードを決める)。
pub(crate) async fn run() -> Result<(), String> {
    let cfg = load_llm_config()?;

    let ondemand = select_role(&cfg, "ondemand")
        .ok_or_else(|| "LLM 設定に ondemand も provider もありません".to_string())?;
    validate_provider(ondemand)?;

    // deferred が無ければ ondemand へフォールバックする(Backend::initialize と同じ挙動)。
    let deferred = match select_role(&cfg, "deferred") {
        Some(v) => {
            validate_provider(v)?;
            v
        }
        None => {
            warn!("llm.deferred is not configured; chat digest falls back to the ondemand config");
            ondemand
        }
    };

    let state = Arc::new(AgentState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        llm: Arc::new(tokio::sync::Mutex::new(Some(crate::llm::build_client(
            ondemand,
            "system_chat.md",
        )))),
        digest_llm: Arc::new(tokio::sync::Mutex::new(Some(crate::llm::build_client(
            deferred,
            "system_chat.md",
        )))),
        next_session_no: std::sync::atomic::AtomicU64::new(0),
    });

    info!("start acp agent");

    let init_state = state.clone();
    let session_state = state.clone();
    let prompt_state = state.clone();

    Agent
        .builder()
        .name("fifty-four")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _connection| {
                let _ = &init_state;
                debug!("acp initialize: protocol_version={:?}", req.protocol_version);
                // テキストの送受信は baseline なので追加の capability 宣言は要らない。
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _connection| {
                let id = session_state.new_session_id();
                debug!("acp session/new: id={} cwd={:?}", id, req.cwd);
                session_state.sessions.lock().await.insert(
                    id.clone(),
                    Session {
                        root: req.cwd.clone(),
                        turns: Vec::new(),
                    },
                );
                responder.respond(NewSessionResponse::new(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let session_id = req.session_id.clone();
                let message = prompt_text(&req);
                debug!("acp session/prompt: id={} {} chars", session_id, message.len());

                // 履歴へ積み、応答生成に必要な情報だけ取り出してロックを手放す。
                let context = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    match sessions.get_mut(&session_id) {
                        Some(s) => {
                            s.turns.push(ChatTurn {
                                speaker: Speaker::Author,
                                text: message.clone(),
                            });
                            Some((s.root.clone(), render_history(&s.turns, MAX_HISTORY_TURNS)))
                        }
                        None => None,
                    }
                };

                let Some((root, history)) = context else {
                    warn!("acp session/prompt: unknown session {}", session_id);
                    return responder
                        .respond_with_internal_error(format!("unknown session: {}", session_id));
                };

                let reply = match generate_reply(&prompt_state.llm, &history, &message).await {
                    Ok(r) => r,
                    Err(e) => {
                        error!("acp chat failed: {}", e);
                        return responder
                            .respond_with_internal_error(format!("LLM 応答の生成に失敗: {}", e));
                    }
                };

                // `chat()` がストリーミングを公開していないため、1ターン1チャンクで送る。
                connection.send_notification(SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(reply.clone().into())),
                ))?;

                // 応答を履歴へ積み、要約の材料を取り出す。
                let digest_turns = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    match sessions.get_mut(&session_id) {
                        Some(s) => {
                            s.turns.push(ChatTurn {
                                speaker: Speaker::Agent,
                                text: reply,
                            });
                            Some(s.turns.clone())
                        }
                        None => None,
                    }
                };

                // 要約はチャットの応答をブロックしない。失敗しても会話は続けられる。
                if let Some(turns) = digest_turns {
                    let digest_llm = prompt_state.digest_llm.clone();
                    tokio::spawn(async move {
                        update_digest(&digest_llm, &root, &turns).await;
                    });
                }

                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // 未処理のリクエストは Method not found、未処理の通知(session/cancel 等)は
        // 無視、がライブラリ側の既定動作なので追加のハンドラは要らない。
        .connect_to(Stdio::new())
        .await
        .map_err(|e| format!("ACP connection failed: {}", e))
}

/// `PromptRequest` からテキストだけを取り出して連結する。
///
/// 画像・音声・リソースは capability を宣言していないので届かない想定だが、
/// 届いても落とさずに無視する。
pub(crate) fn prompt_text(req: &PromptRequest) -> String {
    req.prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 会話履歴を新しい方から `max_turns` 件だけプロンプト用に整形する。
pub(crate) fn render_history(turns: &[ChatTurn], max_turns: usize) -> String {
    let start = turns.len().saturating_sub(max_turns);
    turns[start..]
        .iter()
        .map(|t| format!("{}: {}", t.speaker.label(), t.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// チャットの応答を生成する。
async fn generate_reply(llm: &LlmSlot, history: &str, message: &str) -> Result<String, LlmError> {
    let (template, options) = crate::frontmatter::load_prompt("prompt_chat.md").ok_or_else(|| {
        LlmError::GenericError {
            message: "prompt_chat.md not found".to_string(),
        }
    })?;

    let vars = HashMap::from([("HISTORY", history), ("MESSAGE", message)]);
    let prompt = crate::frontmatter::expand(&template, &vars);

    crate::llm::use_llm_with_option(llm, options, async |l| {
        l.add(Content::Text(prompt));
        l.chat().await
    })
    .await
}

/// 会話を要約して受け渡しファイルへ書き出す。
///
/// 失敗はログに落とすだけで握りつぶす。要約が無くても補完は
/// `{{CHAT}}` が空になるだけで動く。
async fn update_digest(llm: &LlmSlot, root: &std::path::Path, turns: &[ChatTurn]) {
    let Some((template, options)) = crate::frontmatter::load_prompt("prompt_chat_digest.md") else {
        warn!("prompt_chat_digest.md not found; chat context will not be updated");
        return;
    };

    let history = render_history(turns, MAX_DIGEST_TURNS);
    let vars = HashMap::from([("HISTORY", history.as_str())]);
    let prompt = crate::frontmatter::expand(&template, &vars);

    let digest = crate::llm::use_llm_with_option(llm, options, async |l| {
        l.add(Content::Text(prompt));
        l.chat().await
    })
    .await;

    match digest {
        Ok(text) => {
            let text = text.trim();
            if text.is_empty() {
                debug!("chat digest is empty; keeping the previous one");
                return;
            }
            match crate::chat_context::write_digest(root, text) {
                Ok(()) => debug!("chat digest updated ({} chars)", text.chars().count()),
                Err(e) => warn!("failed to write chat digest: {}", e),
            }
        }
        Err(e) => warn!("failed to generate chat digest: {}", e),
    }
}

/// LLM 設定を読む。
///
/// `agent_servers` のエントリには `command`/`args`/`env` しか無く、LSP のように
/// `initialization_options` を受け取れない。そのため環境変数か CLI 引数で渡す。
/// 形式は LSP の `initialization_options.llm` と同じ
/// (`{"ondemand": {...}, "deferred": {...}}`、旧形式の `{"provider": ...}` も可)。
/// API キーは従来どおり `genai` がプロセス環境変数から読むので、ここには含めない。
fn load_llm_config() -> Result<serde_json::Value, String> {
    const ENV_KEY: &str = "FIFTY_FOUR_LLM_CONFIG";

    if let Ok(raw) = std::env::var(ENV_KEY) {
        return serde_json::from_str(&raw)
            .map_err(|e| format!("{} の JSON が不正です: {}", ENV_KEY, e));
    }

    if let Some(path) = arg_value("--llm-config") {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("--llm-config {} が読めません: {}", path, e))?;
        return serde_json::from_str(&raw)
            .map_err(|e| format!("--llm-config {} の JSON が不正です: {}", path, e));
    }

    Err(format!(
        "LLM 設定がありません。環境変数 {} か --llm-config <path> を指定してください",
        ENV_KEY
    ))
}

/// `--name value` / `--name=value` の形でコマンドライン引数を1つ取り出す。
///
/// 引数は `--acp` とこれだけなので、依存を増やしてまで clap は入れない。
fn arg_value(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    let prefix = format!("{}=", name);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(v) = arg.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}

/// 設定ルートから用途別(`ondemand`/`deferred`)の設定を取り出す。
///
/// 旧形式(ルート直下に `provider`)は `ondemand` として扱う。
/// `Backend::initialize` の互換処理と同じ規則。
fn select_role<'a>(root: &'a serde_json::Value, role: &str) -> Option<&'a serde_json::Value> {
    root.get(role).or_else(|| {
        if role == "ondemand" && root.get("provider").is_some() {
            Some(root)
        } else {
            None
        }
    })
}

/// `LlmClientBuilder::from_value` は `provider` 欠落・未対応値で panic するため、
/// クライアントを組む前に確かめる。
fn validate_provider(cfg: &serde_json::Value) -> Result<(), String> {
    let name = cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "LLM 設定に provider がありません".to_string())?;
    crate::llm::Provider::from_str(name).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::TextContent;

    fn turn(speaker: Speaker, text: &str) -> ChatTurn {
        ChatTurn {
            speaker,
            text: text.to_string(),
        }
    }

    #[test]
    fn test_prompt_text_joins_text_blocks() {
        let req = PromptRequest::new(
            SessionId::new("s1"),
            vec![
                ContentBlock::Text(TextContent::new("第3章の別れの場面を書きたい")),
                ContentBlock::Text(TextContent::new("雨の描写を入れたい")),
            ],
        );
        assert_eq!(
            prompt_text(&req),
            "第3章の別れの場面を書きたい\n雨の描写を入れたい"
        );
    }

    #[test]
    fn test_prompt_text_ignores_non_text_blocks() {
        let req = PromptRequest::new(
            SessionId::new("s1"),
            vec![ContentBlock::Text(TextContent::new("テキストだけ残る"))],
        );
        assert_eq!(prompt_text(&req), "テキストだけ残る");
    }

    #[test]
    fn test_render_history_labels_speakers() {
        let turns = vec![
            turn(Speaker::Author, "雨の場面にしたい"),
            turn(Speaker::Agent, "傘を差さない描写はどうでしょう"),
        ];
        assert_eq!(
            render_history(&turns, 10),
            "作者: 雨の場面にしたい\n\nアシスタント: 傘を差さない描写はどうでしょう"
        );
    }

    #[test]
    fn test_render_history_keeps_the_newest_turns() {
        let turns = vec![
            turn(Speaker::Author, "1"),
            turn(Speaker::Agent, "2"),
            turn(Speaker::Author, "3"),
        ];
        // 直近2件だけ残ること(古い方から捨てる)
        assert_eq!(render_history(&turns, 2), "アシスタント: 2\n\n作者: 3");
    }

    #[test]
    fn test_render_history_empty() {
        assert_eq!(render_history(&[], 5), "");
    }

    #[test]
    fn test_select_role_new_format() {
        let cfg = serde_json::json!({
            "ondemand": {"provider": "google"},
            "deferred": {"provider": "openai"},
        });
        assert_eq!(
            select_role(&cfg, "ondemand").unwrap()["provider"],
            serde_json::json!("google")
        );
        assert_eq!(
            select_role(&cfg, "deferred").unwrap()["provider"],
            serde_json::json!("openai")
        );
    }

    #[test]
    fn test_select_role_legacy_flat_format_is_ondemand() {
        let cfg = serde_json::json!({"provider": "google", "model": "gemini-x"});
        assert!(select_role(&cfg, "ondemand").is_some());
        // 旧形式には deferred が無い → 呼び出し側が ondemand へフォールバックする
        assert!(select_role(&cfg, "deferred").is_none());
    }

    #[test]
    fn test_validate_provider_rejects_missing_and_unknown() {
        assert!(validate_provider(&serde_json::json!({"provider": "google"})).is_ok());
        assert!(validate_provider(&serde_json::json!({"model": "x"})).is_err());
        assert!(validate_provider(&serde_json::json!({"provider": "nonexistent"})).is_err());
    }
}
