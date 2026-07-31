//! ACP (Agent Client Protocol) プロキシ。
//!
//! `fifty_four_lsp --acp` で起動したときのモード。Zed と作者が普段使っている
//! ACP エージェント(Claude Code や Gemini CLI など)の**あいだに挟まり**、
//! 会話を素通しさせながら覗き見る。
//!
//! 目的はチャット機能を提供することではなく、**作者が「いま何を書こうとしているか」を
//! LSP の短文生成へ渡す**こと。1ターンごとに会話を要約して [`crate::chat_context`] の
//! ファイルへ書き出し、LSP 側が補完・code action のプロンプトへ `{{CHAT}}` として埋め込む。
//!
//! チャットの応答そのものは上流エージェントが作るので、この実装は応答を生成しない。
//! 作者は普段どおりツール実行やファイル閲覧のできるエージェントと話せる。
//!
//! # 構成
//!
//! ```text
//! Zed ──stdio──> fifty_four_lsp --acp
//!                  └─ ConductorImpl
//!                       ├─ FiftyFourProxy  (このモジュール)
//!                       └─ AcpAgent        (上流エージェントのプロセス)
//! ```
//!
//! プロキシは上流エージェントへ直結できない。プロキシが agent 方向へ送るメッセージは
//! `SuccessorMessage` エンベロープに包まれ、素のエージェントはそれを解釈できないため。
//! 包み・解きは conductor の役目なので、`ConductorImpl` をライブラリとして
//! このプロセスに埋め込んでいる(Zed から見れば `agent_servers` エントリは1つのまま)。
//!
//! # Zed 側の設定
//!
//! Zed の拡張 API には ACP エージェントを登録する口が無い(language server /
//! MCP context server / DAP のみ)。ユーザの `settings.json` に手で書く必要がある。
//! 詳細は `docs/acp-agent.md` を参照。

use crate::llm::{Content, LlmInterface};
use agent_client_protocol::schema::v1::{
    ContentBlock, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate,
};
use agent_client_protocol::{
    AcpAgent, Agent, Client, ConnectTo, Conductor, Handled, Proxy, Stdio,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 要約に渡す過去ターン数の上限。
///
/// 拾うべきなのは「いま書こうとしている場面」なので、会話全体を渡す必要はない。
const MAX_DIGEST_TURNS: usize = 8;

/// LLM クライアントの置き場所。`Backend` と同じ形にして
/// [`crate::llm::use_llm_with_option`] をそのまま使えるようにする。
type LlmSlot = Arc<tokio::sync::Mutex<Option<Box<dyn LlmInterface>>>>;

/// 会話の話者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    /// 作者(Zed の Agent Panel で入力した人)
    Author,
    /// 上流の ACP エージェント
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

/// セッションごとの観測結果。
#[derive(Debug, Default)]
struct Session {
    /// `session/new` で渡されたワークスペースルート。要約の書き出し先の決定に使う。
    root: PathBuf,
    turns: Vec<ChatTurn>,
    /// 進行中のターンで上流から流れてきた `AgentMessageChunk` の連結。
    /// `session/prompt` の応答が返った時点で1ターンとして確定する。
    pending_reply: String,
}

/// ハンドラ間で共有する状態。
#[derive(Debug)]
struct ProxyState {
    sessions: parking_lot::Mutex<HashMap<SessionId, Session>>,
    /// 要約の生成用(`llm.deferred`)。チャット応答は上流が作るので、
    /// このプロセスが LLM を呼ぶのは要約のときだけ。
    digest_llm: LlmSlot,
}

impl ProxyState {
    /// 作者の発話を記録する。セッションが未登録なら何もしない。
    fn push_author(&self, id: &SessionId, text: String) {
        if let Some(s) = self.sessions.lock().get_mut(id) {
            s.turns.push(ChatTurn {
                speaker: Speaker::Author,
                text,
            });
        }
    }

    /// 上流の応答チャンクを進行中ターンへ積む。
    fn push_reply_chunk(&self, id: &SessionId, text: &str) {
        if let Some(s) = self.sessions.lock().get_mut(id) {
            s.pending_reply.push_str(text);
        }
    }

    /// 進行中のターンを確定し、要約に渡す材料を返す。
    ///
    /// 応答が空(ツール実行だけで終わったターン等)の場合も、作者の発話は
    /// 既に積まれているので要約は行う。
    fn finish_turn(&self, id: &SessionId) -> Option<(PathBuf, Vec<ChatTurn>)> {
        let mut sessions = self.sessions.lock();
        let s = sessions.get_mut(id)?;
        let reply = std::mem::take(&mut s.pending_reply);
        let reply = reply.trim();
        if !reply.is_empty() {
            s.turns.push(ChatTurn {
                speaker: Speaker::Agent,
                text: reply.to_string(),
            });
        }
        Some((s.root.clone(), s.turns.clone()))
    }
}

/// conductor へ差し込むプロキシ部品。
struct FiftyFourProxy {
    state: Arc<ProxyState>,
}

impl ConnectTo<Conductor> for FiftyFourProxy {
    async fn connect_to(
        self,
        peer: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let new_session = self.state.clone();
        let prompt = self.state.clone();
        let notify = self.state.clone();

        Proxy
            .builder()
            .name("fifty-four")
            // `session/new`: cwd を控えて素通しし、上流が採番した SessionId と結び付ける。
            .on_receive_request_from(
                Client,
                async move |req: NewSessionRequest, responder, cx| {
                    let root = req.cwd.clone();
                    let state = new_session.clone();
                    let cancellation = responder.cancellation();

                    cx.send_request_to(Agent, req)
                        .forward_cancellation_from(cancellation)
                        .on_receiving_result(move |result: Result<NewSessionResponse, _>| async move {
                            if let Ok(res) = &result {
                                debug!("acp session/new: id={} cwd={:?}", res.session_id, root);
                                state.sessions.lock().insert(
                                    res.session_id.clone(),
                                    Session {
                                        root,
                                        ..Default::default()
                                    },
                                );
                            }
                            responder.respond_with_result(result)
                        })
                },
                agent_client_protocol::on_receive_request!(),
            )
            // `session/prompt`: 作者の発話を控えて素通しし、応答が返った時点で
            // 1ターン確定 → 要約を投げる。
            .on_receive_request_from(
                Client,
                async move |req: PromptRequest, responder, cx| {
                    let session_id = req.session_id.clone();
                    let message = prompt_text(&req);
                    debug!(
                        "acp session/prompt: id={} {} chars",
                        session_id,
                        message.len()
                    );
                    prompt.push_author(&session_id, message);

                    let state = prompt.clone();
                    let cancellation = responder.cancellation();

                    cx.send_request_to(Agent, req)
                        .forward_cancellation_from(cancellation)
                        .on_receiving_result(move |result: Result<PromptResponse, _>| async move {
                            // 応答をクライアントへ返すのが先。要約はそのあと
                            // バックグラウンドで行い、作者を待たせない。
                            let turn = state.finish_turn(&session_id);
                            let responded = responder.respond_with_result(result);

                            if let Some((root, turns)) = turn {
                                debug!(
                                    "acp turn finished: id={} turns={} root={:?}",
                                    session_id,
                                    turns.len(),
                                    root
                                );
                                let digest_llm = state.digest_llm.clone();
                                tokio::spawn(async move {
                                    update_digest(&digest_llm, &root, &turns).await;
                                });
                            }
                            responded
                        })
                },
                agent_client_protocol::on_receive_request!(),
            )
            // `session/update`: 応答チャンクを覗くだけ。`Handled::No` を返して
            // 既定の転送処理へそのまま流す(書き換えない)。
            .on_receive_notification_from(
                Agent,
                async move |notif: SessionNotification, cx| {
                    if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update
                        && let ContentBlock::Text(t) = &chunk.content
                    {
                        notify.push_reply_chunk(&notif.session_id, &t.text);
                    }
                    Ok(Handled::No {
                        message: (notif, cx),
                        retry: false,
                    })
                },
                agent_client_protocol::on_receive_notification!(),
            )
            // 上記以外は既定の転送に任せる(プロキシは未処理メッセージを素通しする)。
            .connect_to(peer)
            .await
    }
}

/// ACP プロキシとして stdio で待ち受ける。
///
/// 上流エージェントや LLM 設定が読めない場合はここで `Err` を返す
/// (呼び出し元が終了コードを決める)。
pub(crate) async fn run() -> Result<(), String> {
    let upstream = load_upstream_agent()?;

    let cfg = load_llm_config()?;
    let digest_cfg = select_digest_config(&cfg)
        .ok_or_else(|| "LLM 設定に deferred も ondemand も provider もありません".to_string())?;
    validate_provider(digest_cfg)?;

    let state = Arc::new(ProxyState {
        sessions: parking_lot::Mutex::new(HashMap::new()),
        digest_llm: Arc::new(tokio::sync::Mutex::new(Some(crate::llm::build_client(
            digest_cfg,
            "system.md",
        )))),
    });

    info!("start acp proxy");

    ConductorImpl::new_agent(
        "fifty-four",
        ProxiesAndAgent::new(upstream).proxy(FiftyFourProxy { state }),
    )
    .run(Stdio::new())
    .await
    .map_err(|e| format!("ACP proxy failed: {}", e))
}

/// `PromptRequest` からテキストだけを取り出して連結する。
///
/// 画像・音声・リソースリンクは上流エージェントへは素通しするが、要約の材料には
/// しない(テキストだけで「何を書こうとしているか」は足りる)。
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

/// 中継先の ACP エージェントを決める。
///
/// `AcpAgent::from_str` はコマンド文字列(`"npx -y @agentclientprotocol/claude-agent-acp@latest"`)
/// と JSON 設定(`{"type":"stdio","command":...}`)のどちらも受け付ける。
fn load_upstream_agent() -> Result<AcpAgent, String> {
    const ENV_KEY: &str = "FIFTY_FOUR_ACP_AGENT";

    let spec = std::env::var(ENV_KEY)
        .ok()
        .or_else(|| arg_value("--agent"))
        .ok_or_else(|| {
            format!(
                "中継先の ACP エージェントが指定されていません。環境変数 {} か --agent <command> を指定してください",
                ENV_KEY
            )
        })?;

    spec.parse::<AcpAgent>()
        .map_err(|e| format!("ACP エージェントの指定が不正です ({}): {}", spec, e))
}

/// 要約用の LLM 設定を読む。
///
/// `agent_servers` のエントリには `command`/`args`/`env` しか無く、LSP のように
/// `initialization_options` を受け取れない。そのため環境変数か CLI 引数で渡す。
/// 形式は LSP の `initialization_options.llm` と同じ。API キーは従来どおり
/// `genai` がプロセス環境変数から読むので、ここには含めない。
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
        "要約用の LLM 設定がありません。環境変数 {} か --llm-config <path> を指定してください",
        ENV_KEY
    ))
}

/// `--name value` / `--name=value` の形でコマンドライン引数を1つ取り出す。
///
/// 引数の数が少ないので、依存を増やしてまで clap は入れない。
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

/// 要約に使う設定を選ぶ。
///
/// このプロセスが LLM を呼ぶのは要約だけなので `deferred` を優先する。
/// 無ければ `ondemand`、それも無ければ旧形式(ルート直下に `provider`)を使う。
fn select_digest_config(root: &serde_json::Value) -> Option<&serde_json::Value> {
    root.get("deferred")
        .or_else(|| root.get("ondemand"))
        .or_else(|| {
            if root.get("provider").is_some() {
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

    fn state() -> ProxyState {
        ProxyState {
            sessions: parking_lot::Mutex::new(HashMap::new()),
            digest_llm: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    fn registered(root: &str) -> (ProxyState, SessionId) {
        let s = state();
        let id = SessionId::new("s1");
        s.sessions.lock().insert(
            id.clone(),
            Session {
                root: PathBuf::from(root),
                ..Default::default()
            },
        );
        (s, id)
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

    /// 1ターン: 作者の発話 → チャンク2つ → 確定、で2発話になること。
    #[test]
    fn test_observed_turn_becomes_two_turns() {
        let (s, id) = registered("/ws");
        s.push_author(&id, "雨の場面にしたい".to_string());
        s.push_reply_chunk(&id, "傘を差さない");
        s.push_reply_chunk(&id, "描写はどうでしょう");

        let (root, turns) = s.finish_turn(&id).unwrap();
        assert_eq!(root, PathBuf::from("/ws"));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].speaker, Speaker::Author);
        assert_eq!(turns[1].speaker, Speaker::Agent);
        // チャンクが連結されていること
        assert_eq!(turns[1].text, "傘を差さない描写はどうでしょう");
    }

    /// 確定後にバッファが空になり、次のターンへ持ち越されないこと。
    #[test]
    fn test_finish_turn_clears_pending_reply() {
        let (s, id) = registered("/ws");
        s.push_author(&id, "一回目".to_string());
        s.push_reply_chunk(&id, "応答1");
        s.finish_turn(&id).unwrap();

        s.push_author(&id, "二回目".to_string());
        s.push_reply_chunk(&id, "応答2");
        let (_, turns) = s.finish_turn(&id).unwrap();

        assert_eq!(turns.len(), 4);
        assert_eq!(turns[3].text, "応答2", "前ターンの応答が混ざっている");
    }

    /// 応答が無いターン(ツール実行だけで終わった等)でも作者の発話は残ること。
    #[test]
    fn test_turn_without_reply_keeps_author_turn() {
        let (s, id) = registered("/ws");
        s.push_author(&id, "ファイルを見て".to_string());

        let (_, turns) = s.finish_turn(&id).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker, Speaker::Author);
    }

    /// 未登録セッションは無視し、panic しないこと。
    #[test]
    fn test_unknown_session_is_ignored() {
        let s = state();
        let unknown = SessionId::new("nope");
        s.push_author(&unknown, "やあ".to_string());
        s.push_reply_chunk(&unknown, "やあ");
        assert!(s.finish_turn(&unknown).is_none());
    }

    #[test]
    fn test_select_digest_config_prefers_deferred() {
        let cfg = serde_json::json!({
            "ondemand": {"provider": "google"},
            "deferred": {"provider": "openai"},
        });
        assert_eq!(
            select_digest_config(&cfg).unwrap()["provider"],
            serde_json::json!("openai")
        );
    }

    #[test]
    fn test_select_digest_config_falls_back_to_ondemand() {
        let cfg = serde_json::json!({"ondemand": {"provider": "google"}});
        assert_eq!(
            select_digest_config(&cfg).unwrap()["provider"],
            serde_json::json!("google")
        );
    }

    #[test]
    fn test_select_digest_config_legacy_flat_format() {
        let cfg = serde_json::json!({"provider": "google", "model": "gemini-x"});
        assert!(select_digest_config(&cfg).is_some());
        assert!(select_digest_config(&serde_json::json!({"model": "x"})).is_none());
    }

    #[test]
    fn test_validate_provider_rejects_missing_and_unknown() {
        assert!(validate_provider(&serde_json::json!({"provider": "google"})).is_ok());
        assert!(validate_provider(&serde_json::json!({"model": "x"})).is_err());
        assert!(validate_provider(&serde_json::json!({"provider": "nonexistent"})).is_err());
    }
}
