//! ACP (Agent Client Protocol) エージェント。
//!
//! `fifty_four_lsp --acp` で起動したときのモード。Zed の Agent Panel から stdio 越しに
//! 接続され、作者の相談相手として応答する。中身は Claude Agent SDK
//! ([`crate::writing_agent`])で、原稿ディレクトリのファイルを読み書きしながら話す。
//!
//! 目的はチャット機能そのものではなく、**作者が「いま何を書こうとしているか」を
//! LSP の短文生成へ渡す**こと。1ターンごとに会話を要約して [`crate::chat_context`] の
//! ファイルへ書き出し、LSP 側が補完・code action のプロンプトへ `{{CHAT}}` として埋め込む。
//!
//! # 構成
//!
//! ```text
//! Zed ──stdio──> fifty_four_lsp --acp
//!                  └─ ClaudeAgent (Claude Agent SDK → claude CLI)
//!                       └── <workspace>/.fifty_four/chat_context.md
//!                              ↑ LSP サーバ(別プロセス)が補完時に読む
//! ```
//!
//! Zed は LSP サーバ(拡張経由)と ACP エージェント(`agent_servers` 設定経由)を
//! **別プロセス**として起動する。Zed の拡張 API には ACP を登録する口が無い
//! (language server / MCP context server / DAP のみ)ため、この分離は避けられない。
//! 受け渡しは `.fifty_four/chat_context.md` の1ファイルで、`plot.md` を毎回読み直す
//! [`crate::tools`] と同じ方式にしている。
//!
//! # 認証と debug ビルド限定であること
//!
//! LLM アクセスは Claude Agent SDK 経由、つまり `claude` CLI の認証をそのまま使う。
//! このモジュールは API キーを要求しないし、自前で持つこともしない。それどころか
//! `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` が環境にあっても `main` が起動前に
//! 取り除く(`scrub_anthropic_credentials`)。残しておくとサブスクリプション枠ではなく
//! API クレジット課金になってしまうため。
//!
//! 作者自身のサブスクリプション枠を第三者へ提供することは Anthropic の規約上できないので、
//! このモジュールごと `#[cfg(debug_assertions)]` で release ビルドから落としてある
//! (`main.rs` を参照)。

use crate::writing_agent::{AgentError, ClaudeAgent, WritingAgent};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, Stdio};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 要約に渡す過去ターン数の上限。
///
/// 拾うべきなのは「いま書こうとしている場面」なので、会話全体を渡す必要はない。
const MAX_DIGEST_TURNS: usize = 8;

/// 接続が切れたあと、走っている要約タスクを待つ上限。
///
/// 要約は `session/prompt` の応答を返したあとに走るので、その直後に Zed が切断すると
/// 書き終える前にランタイムごと落ちる。ここで待つことで取りこぼしを防ぐ。
const DIGEST_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 会話の話者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    /// 作者(Zed の Agent Panel で入力した人)
    Author,
    /// 執筆相談エージェント
    Agent,
}

impl Speaker {
    /// 要約プロンプトへ書き出すときのラベル。
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
struct Session {
    /// `session/new` で渡されたワークスペースルート。要約の書き出し先の決定に使う。
    root: PathBuf,
    agent: Arc<dyn WritingAgent>,
    turns: Vec<ChatTurn>,
}

/// ハンドラ間で共有する状態。
struct AgentState {
    sessions: tokio::sync::Mutex<HashMap<SessionId, Session>>,
    /// 実行中の要約タスク。切断時に待ち合わせるため保持する([`DIGEST_DRAIN_TIMEOUT`])。
    digests: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
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
pub(crate) async fn run() -> Result<(), String> {
    // 起動時に読めないと全セッションが失敗するので、ここで確かめておく。
    let _ = system_prompt()?;

    let state = Arc::new(AgentState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        digests: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
        next_session_no: std::sync::atomic::AtomicU64::new(0),
    });

    info!("start acp agent");

    let new_session = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state.clone();

    let result = Agent
        .builder()
        .name("fifty-four")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                debug!("acp initialize: protocol_version={:?}", req.protocol_version);
                // テキストの送受信は baseline なので追加の capability 宣言は要らない。
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/new`: ワークスペースに紐づくエージェント(= claude プロセス)を1つ起こす。
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                let id = new_session.new_session_id();
                debug!("acp session/new: id={} cwd={:?}", id, req.cwd);

                let prompt = match system_prompt() {
                    Ok(p) => p,
                    Err(e) => return responder.respond_with_internal_error(e),
                };
                let agent = match ClaudeAgent::start(&req.cwd, prompt).await {
                    Ok(a) => Arc::new(a) as Arc<dyn WritingAgent>,
                    Err(e) => {
                        error!("failed to start writing agent: {}", e);
                        return responder
                            .respond_with_internal_error(format!("エージェントを起動できません: {}", e));
                    }
                };

                new_session.sessions.lock().await.insert(
                    id.clone(),
                    Session {
                        root: req.cwd.clone(),
                        agent,
                        turns: Vec::new(),
                    },
                );
                responder.respond(NewSessionResponse::new(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/prompt`: エージェントへ中継し、応答を返してから要約を更新する。
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let session_id = req.session_id.clone();
                let message = prompt_text(&req);
                debug!(
                    "acp session/prompt: id={} {} chars",
                    session_id,
                    message.len()
                );

                // 必要なものだけ取り出してロックを手放す。ターンの間ずっと
                // sessions を掴んでいると、別セッションの処理まで止まってしまう。
                let session = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    match sessions.get_mut(&session_id) {
                        Some(s) => {
                            s.turns.push(ChatTurn {
                                speaker: Speaker::Author,
                                text: message.clone(),
                            });
                            Some((s.root.clone(), s.agent.clone()))
                        }
                        None => None,
                    }
                };

                let Some((root, agent)) = session else {
                    warn!("acp session/prompt: unknown session {}", session_id);
                    return responder
                        .respond_with_internal_error(format!("unknown session: {}", session_id));
                };

                // 届いたそばからクライアントへ流す。送信に失敗したら以降は諦めて
                // 全文だけ組み立てる(応答自体は返せるため)。
                let mut send_error: Option<agent_client_protocol::Error> = None;
                let reply = {
                    let mut on_chunk = |piece: String| {
                        if send_error.is_some() {
                            return;
                        }
                        if let Err(e) = connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(piece.into())),
                        )) {
                            send_error = Some(e);
                        }
                    };
                    agent.prompt(&message, &mut on_chunk).await
                };

                if let Some(e) = send_error {
                    warn!("acp: failed to stream chunk: {}", e);
                }

                let reply = match reply {
                    Ok(r) => r,
                    Err(AgentError { message }) => {
                        error!("acp prompt failed: {}", message);
                        return responder
                            .respond_with_internal_error(format!("応答の生成に失敗: {}", message));
                    }
                };

                // 応答を履歴へ積み、要約の材料を取り出す。
                let digest_input = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    match sessions.get_mut(&session_id) {
                        Some(s) => {
                            let reply = reply.trim();
                            if !reply.is_empty() {
                                s.turns.push(ChatTurn {
                                    speaker: Speaker::Agent,
                                    text: reply.to_string(),
                                });
                            }
                            Some(s.turns.clone())
                        }
                        None => None,
                    }
                };

                // 応答を返すのが先。要約はそのあとバックグラウンドで行い、作者を待たせない。
                let responded = responder.respond(PromptResponse::new(StopReason::EndTurn));

                if let Some(turns) = digest_input {
                    debug!(
                        "acp turn finished: id={} turns={} root={:?}",
                        session_id,
                        turns.len(),
                        root
                    );
                    let mut digests = prompt_state.digests.lock().await;
                    // 完了済みを回収してから積む(放っておくと JoinSet が伸び続ける)。
                    while digests.try_join_next().is_some() {}
                    digests.spawn(async move {
                        update_digest(&root, &turns).await;
                    });
                }
                responded
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/cancel`: 進行中のターンを止める。
        .on_receive_notification(
            async move |notif: CancelNotification, _cx| {
                debug!("acp session/cancel: id={}", notif.session_id);
                let agent = cancel_state
                    .sessions
                    .lock()
                    .await
                    .get(&notif.session_id)
                    .map(|s| s.agent.clone());
                if let Some(agent) = agent
                    && let Err(e) = agent.interrupt().await
                {
                    warn!("acp: interrupt failed: {}", e);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;

    // 切断後も、走っている要約は書き終えるまで待つ。
    drain_digests(&state).await;

    result.map_err(|e| format!("ACP connection failed: {}", e))
}

/// 実行中の要約タスクを待ち合わせる。上限を超えたら諦めてログに残す。
async fn drain_digests(state: &AgentState) {
    let mut digests = state.digests.lock().await;
    if digests.is_empty() {
        return;
    }
    debug!("waiting for {} in-flight digest task(s)", digests.len());
    let drained = tokio::time::timeout(DIGEST_DRAIN_TIMEOUT, async {
        while digests.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        warn!("gave up waiting for digest tasks after {:?}", DIGEST_DRAIN_TIMEOUT);
    }
}

/// 会話用のシステムプロンプトを読む。
///
/// Claude Code の既定プロンプトを**置き換える**中身なので、執筆支援としての役割・
/// 原稿ディレクトリの約束事(characters.md / plot.md / memo/*.md)・要約ファイルの
/// 維持義務は全てここに書いてある。
fn system_prompt() -> Result<String, String> {
    crate::assets::load("system_chat.md")
        .ok_or_else(|| "system_chat.md not found on disk nor in embedded assets".to_string())
}

/// `PromptRequest` からテキストだけを取り出して連結する。
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

/// 会話履歴を新しい方から `max_turns` 件だけ要約プロンプト用に整形する。
pub(crate) fn render_history(turns: &[ChatTurn], max_turns: usize) -> String {
    let start = turns.len().saturating_sub(max_turns);
    turns[start..]
        .iter()
        .map(|t| format!("{}: {}", t.speaker.label(), t.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 会話を要約して受け渡しファイルへ書き出す。
///
/// 対話用とは別セッションの一発問い合わせで回す。会話側のクライアントを占有しないので、
/// 要約中でも次のターンを受けられる。失敗はログに落とすだけで握りつぶす —
/// 要約が無くても補完は `{{CHAT}}` が空になるだけで動く。
async fn update_digest(root: &std::path::Path, turns: &[ChatTurn]) {
    let Some((template, _options)) = crate::frontmatter::load_prompt("prompt_chat_digest.md")
    else {
        warn!("prompt_chat_digest.md not found; chat context will not be updated");
        return;
    };

    let history = render_history(turns, MAX_DIGEST_TURNS);
    let vars = HashMap::from([("HISTORY", history.as_str())]);
    let prompt = crate::frontmatter::expand(&template, &vars);

    let system = match crate::assets::load("system_chat_digest.md") {
        Some(s) => s,
        None => {
            warn!("system_chat_digest.md not found; chat context will not be updated");
            return;
        }
    };

    match ClaudeAgent::one_shot(root, system, prompt).await {
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
        assert_eq!(render_history(&turns, 2), "アシスタント: 2\n\n作者: 3");
    }

    #[test]
    fn test_render_history_empty() {
        assert_eq!(render_history(&[], 5), "");
    }

    /// システムプロンプトは埋め込みアセットから必ず読めること。
    /// (読めないと全セッションが起動時に失敗する)
    #[test]
    fn test_system_prompt_is_available() {
        assert!(system_prompt().is_ok());
        assert!(crate::assets::load("system_chat_digest.md").is_some());
    }
}
