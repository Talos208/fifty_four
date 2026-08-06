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
//! # 認証
//!
//! LLM アクセスは Claude Agent SDK 経由、つまり `claude` CLI の認証をそのまま使う。
//! `ANTHROPIC_API_KEY` を設定しなければ、ログイン済み CLI のサブスクリプション枠で動く。
//! このモジュールは API キーを要求しないし、自前で持つこともしない。

use crate::acp_config::{self, SessionConfig};
use crate::writing_agent::{AgentError, ClaudeAgent, WritingAgent};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, Meta, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SessionCapabilities, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, UsageUpdate,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
///
/// [`crate::session_log`] がそのまま1行のJSONとして永続化する
/// (`session/load` でのリプレイ用。プロセス再起動をまたいで残る唯一の会話記録)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// 現在の設定(= いま動いている `claude` プロセスに渡した内容)。
    config: SessionConfig,
    /// GUI で変更されたが、まだプロセスへ反映していない設定。
    /// 次の `session/prompt` の頭で適用する
    /// (`anthropic-agent-sdk` はセッション途中の切替を非対応なので、
    /// プロセスを起こし直すタイミングを会話の切れ目まで遅らせている。
    /// [`crate::acp_config`] のモジュールdoc参照)。
    pending: Option<SessionConfig>,
}

/// ハンドラ間で共有する状態。
struct AgentState {
    sessions: tokio::sync::Mutex<HashMap<SessionId, Session>>,
    /// 実行中の要約タスク。切断時に待ち合わせるため保持する([`DIGEST_DRAIN_TIMEOUT`])。
    digests: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
}

impl AgentState {
    /// 新しいセッションIDを採番する。UUID v4形式にする必要がある
    /// (`claude` CLI の `--session-id`/`--resume` がUUIDを要求するため)。
    /// このIDをそのまま ACP の `SessionId` として使い回すので、`session/load` が
    /// 来たときに別途IDのマッピングを持たなくても `claude` CLI 側の永続化済み
    /// セッションへ直接 `--resume` できる。
    fn new_session_id(&self) -> SessionId {
        SessionId::new(uuid::Uuid::new_v4().to_string())
    }
}

/// ACP エージェントとして stdio で待ち受ける。
pub(crate) async fn run() -> Result<(), String> {
    // 起動時に読めないと全セッションが失敗するので、ここで確かめておく。
    let _ = system_prompt()?;

    let state = Arc::new(AgentState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        digests: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
    });

    info!("start acp agent");

    let new_session = state.clone();
    let load_session = state.clone();
    let config_state = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state.clone();

    let result = Agent
        .builder()
        .name("fifty-four")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                debug!(
                    "acp initialize: protocol_version={:?}",
                    req.protocol_version
                );
                responder.respond(
                    InitializeResponse::new(req.protocol_version).agent_capabilities(
                        // `claude` CLI の `--session-id`/`--resume` がそのまま使えるため
                        // `session/load` に対応できる(下のハンドラ参照)。
                        AgentCapabilities::new().load_session(true),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/set_config_option`: モデル/思考レベルの選択を受け付ける。
        // その場では `claude` プロセスを再起動しない — `pending` に積むだけで、
        // 実際の反映は次の `session/prompt` の頭で行う(会話の切れ目まで待つことで
        // 進行中のターンを壊さない)。
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                debug!(
                    "acp session/set_config_option: id={} config_id={:?} value={:?}",
                    req.session_id, req.config_id, req.value
                );
                let mut sessions = config_state.sessions.lock().await;
                let Some(session) = sessions.get_mut(&req.session_id) else {
                    return responder.respond_with_internal_error(format!(
                        "unknown session: {}",
                        req.session_id
                    ));
                };
                let mut next = session
                    .pending
                    .clone()
                    .unwrap_or_else(|| session.config.clone());
                if let Err(e) = acp_config::apply(&mut next, req.config_id.0.as_ref(), &req.value) {
                    warn!("acp session/set_config_option: {}", e);
                    return responder.respond_with_internal_error(e);
                }
                let options = acp_config::to_config_options(&next);
                session.pending = Some(next);
                responder.respond(SetSessionConfigOptionResponse::new(options))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/new`: ワークスペースに紐づくエージェント(= claude プロセス)を1つ起こす。
        // ここで採番するIDは `claude` CLI 自身のセッションIDでもある(`--session-id`)ので、
        // `session/load` が同じIDで来たとき素直に `--resume` できる。
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                let id = new_session.new_session_id();
                debug!("acp session/new: id={} cwd={:?}", id, req.cwd);

                // 新しい会話は前の会話の要約を引き継がない。TTLの代わりに
                // ここで明示的に切り替える(lsp/src/chat_context.rs のモジュールdoc参照)。
                if let Err(e) = crate::chat_context::clear(&req.cwd) {
                    warn!("acp: failed to clear chat digest on session/new: {}", e);
                }

                let prompt = match system_prompt() {
                    Ok(p) => p,
                    Err(e) => return responder.respond_with_internal_error(e),
                };
                let config = SessionConfig::default();
                let agent = match ClaudeAgent::start(&req.cwd, prompt, &id.0, false, &config).await
                {
                    Ok(a) => Arc::new(a) as Arc<dyn WritingAgent>,
                    Err(e) => {
                        error!("failed to start writing agent: {}", e);
                        return responder.respond_with_internal_error(format!(
                            "エージェントを起動できません: {}",
                            e
                        ));
                    }
                };

                let config_options = acp_config::to_config_options(&config);
                new_session.sessions.lock().await.insert(
                    id.clone(),
                    Session {
                        root: req.cwd.clone(),
                        agent,
                        turns: Vec::new(),
                        config,
                        pending: None,
                    },
                );
                responder.respond(NewSessionResponse::new(id).config_options(config_options))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/load`: プロセス再起動等でメモリ上の状態が失われたセッションを、
        // `claude` CLI 側の永続化された会話履歴から再開する。
        // `Session::turns` はプロセスのメモリ上にしか無いため、[`crate::session_log`] へ
        // 逐次追記してある過去ターンを読み戻し、ACP の仕様通り `session/update` 通知として
        // 応答の前にリプレイする(仕様は "MUST replay" と明記している)。
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, connection| {
                debug!("acp session/load: id={} cwd={:?}", req.session_id, req.cwd);

                // `claude` CLI の `--resume` はUUID形式のセッションIDしか受け付けない。
                // 今回の実装より前に発行された旧形式(`ff-{pid}-{n}`)のIDがZed側の履歴に
                // 残っていると、UUIDでないIDをそのまま `--resume` に渡すことになり、
                // CLIプロセスが起動直後に引数エラーで自己終了する。その場合
                // `ClaudeAgent::start` 自体は(spawn/connectまでは)成功として返ってしまい、
                // 実際の失敗は次の `session/prompt` での書き込み時に「パイプが閉じている」
                // という分かりにくいエラーとして先送りされる。ここで事前に弾くことで、
                // `session/load` の時点で分かりやすく失敗させる。
                if uuid::Uuid::parse_str(&req.session_id.0).is_err() {
                    warn!(
                        "acp session/load: 不正な形式のセッションID: {}",
                        req.session_id
                    );
                    return responder.respond_with_internal_error(
                        "不明な形式のセッションIDです。新しい会話を開始してください。".to_string(),
                    );
                }

                let prompt = match system_prompt() {
                    Ok(p) => p,
                    Err(e) => return responder.respond_with_internal_error(e),
                };
                // 設定はプロセスのメモリ上にしか無いため、再開時は既定へ戻る
                // (docs/acp-agent.md の「セッションの再開」参照)。
                let config = SessionConfig::default();
                let agent =
                    match ClaudeAgent::start(&req.cwd, prompt, &req.session_id.0, true, &config)
                        .await
                    {
                        Ok(a) => Arc::new(a) as Arc<dyn WritingAgent>,
                        Err(e) => {
                            error!("failed to resume writing agent: {}", e);
                            return responder.respond_with_internal_error(format!(
                                "セッションを再開できません: {}",
                                e
                            ));
                        }
                    };

                // session_log へ逐次追記してきた過去ターンを読み戻す。無ければ
                // (初回・旧セッション・削除済み)空のまま従来通りに始める。
                let turns = crate::session_log::read_turns(&req.cwd, &req.session_id.0);
                debug!(
                    "acp session/load: id={} 過去ターンを{}件読み戻しました",
                    req.session_id,
                    turns.len()
                );

                // ACP の仕様は「応答を返す前に会話全体を session/update でリプレイする」
                // ことを要求している。件数の上限は設けない(ローカルのテキスト再送で
                // あり LLM 呼び出しコストが無いため)。
                for turn in &turns {
                    let update = match turn.speaker {
                        Speaker::Author => SessionUpdate::UserMessageChunk(ContentChunk::new(
                            turn.text.clone().into(),
                        )),
                        Speaker::Agent => SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            turn.text.clone().into(),
                        )),
                    };
                    if let Err(e) = connection
                        .send_notification(SessionNotification::new(req.session_id.clone(), update))
                    {
                        warn!("acp: failed to replay turn on session/load: {}", e);
                        break;
                    }
                }

                // 要約(chat_context.md)をこのセッションへ明示的に切り替える。
                // TTLの代わりに「所有者セッションIDが一致するか」で判断する
                // (lsp/src/chat_context.rs のモジュールdoc参照)。
                match crate::chat_context::owner(&req.cwd) {
                    Some(owner) if owner == req.session_id.0.as_ref() => {
                        // 既にこのセッションの要約が乗っている。直前まで使っていた
                        // スレッドを開き直すだけの最も多いケースなので、
                        // 何もしない(再生成のLLM呼び出しコストをかけない)。
                        debug!(
                            "acp session/load: id={} 要約は既にこのセッションの所有です",
                            req.session_id
                        );
                    }
                    _ if turns.is_empty() => {
                        // 復元する材料が無い(旧セッション・削除済みなど)。
                        // 他セッションの要約を誤って残さないよう消しておく。
                        if let Err(e) = crate::chat_context::clear(&req.cwd) {
                            warn!("acp: failed to clear chat digest on session/load: {}", e);
                        }
                    }
                    _ => {
                        // 別セッションが最後に書いた要約が残っている。応答は
                        // 先に返し、要約の再生成はバックグラウンドで追いつかせる
                        // (session/prompt の応答後と同じ方針)。
                        let root = req.cwd.clone();
                        let session_id = req.session_id.0.to_string();
                        let turns_for_digest = turns.clone();
                        let mut digests = load_session.digests.lock().await;
                        while digests.try_join_next().is_some() {}
                        digests.spawn(async move {
                            update_digest(&root, &turns_for_digest, &session_id).await;
                        });
                    }
                }

                let config_options = acp_config::to_config_options(&config);
                load_session.sessions.lock().await.insert(
                    req.session_id.clone(),
                    Session {
                        root: req.cwd.clone(),
                        agent,
                        turns,
                        config,
                        pending: None,
                    },
                );
                responder.respond(LoadSessionResponse::new().config_options(config_options))
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
                            let turn = ChatTurn {
                                speaker: Speaker::Author,
                                text: message.clone(),
                            };
                            if let Err(e) =
                                crate::session_log::append_turn(&s.root, &session_id.0, &turn)
                            {
                                warn!("acp: failed to persist turn: {}", e);
                            }
                            s.turns.push(turn);
                            Some((s.root.clone(), s.agent.clone(), s.pending.clone()))
                        }
                        None => None,
                    }
                };

                let Some((root, mut agent, pending)) = session else {
                    warn!("acp session/prompt: unknown session {}", session_id);
                    return responder
                        .respond_with_internal_error(format!("unknown session: {}", session_id));
                };

                // GUI で選ばれた設定がまだ反映されていなければ、ここ(会話の切れ目)で
                // `claude` プロセスを起こし直す。`anthropic-agent-sdk` はセッション途中の
                // 切替を非対応なので、同じセッションIDで `--resume` することで
                // 会話の文脈を保ったまま設定だけ変える(`session/load` と同じ経路)。
                if let Some(new_config) = pending {
                    let prompt = match system_prompt() {
                        Ok(p) => p,
                        Err(e) => return responder.respond_with_internal_error(e),
                    };
                    match ClaudeAgent::start(&root, prompt, &session_id.0, true, &new_config).await
                    {
                        Ok(new_agent) => {
                            agent = Arc::new(new_agent);
                            let mut sessions = prompt_state.sessions.lock().await;
                            if let Some(s) = sessions.get_mut(&session_id) {
                                s.agent = agent.clone();
                                s.config = new_config;
                                s.pending = None;
                            }
                        }
                        Err(e) => {
                            // 設定を変えられなかっただけで会話を落とす理由は無いので、
                            // 古い agent のまま続行する。pending は残し、次のターンで
                            // 再度試みる。
                            warn!("acp: 設定変更のための再起動に失敗しました: {}", e);
                        }
                    }
                }

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

                // サブスク枠(5時間枠)の使用率が取れていれば、Zed のメーターへ流す。
                //
                // `UsageUpdate` は本来「コンテキストウィンドウの使用量」用のフィールドだが、
                // ACP には枠の残量に対応するフィールドが無いため、意図的にラベルと中身を
                // ずらして流用している(docs/acp-agent.md 参照)。
                if let Some(rate_limit) = &reply.rate_limit {
                    let used = rate_limit.utilization.round().clamp(0.0, 100.0) as u64;
                    let mut usage = UsageUpdate::new(used, 100);
                    if let Some(resets_at) = &rate_limit.resets_at {
                        let mut meta = Meta::new();
                        meta.insert(
                            "resetsAt".to_string(),
                            serde_json::Value::String(resets_at.clone()),
                        );
                        usage = usage.meta(meta);
                    }
                    if let Err(e) = connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::UsageUpdate(usage),
                    )) {
                        warn!("acp: failed to send usage update: {}", e);
                    }
                }

                // 応答を履歴へ積み、要約の材料を取り出す。
                let digest_input = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    match sessions.get_mut(&session_id) {
                        Some(s) => {
                            let reply_text = reply.text.trim();
                            if !reply_text.is_empty() {
                                let turn = ChatTurn {
                                    speaker: Speaker::Agent,
                                    text: reply_text.to_string(),
                                };
                                if let Err(e) =
                                    crate::session_log::append_turn(&s.root, &session_id.0, &turn)
                                {
                                    warn!("acp: failed to persist turn: {}", e);
                                }
                                s.turns.push(turn);
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
                    let owner_id = session_id.0.to_string();
                    digests.spawn(async move {
                        update_digest(&root, &turns, &owner_id).await;
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
        warn!(
            "gave up waiting for digest tasks after {:?}",
            DIGEST_DRAIN_TIMEOUT
        );
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
async fn update_digest(root: &std::path::Path, turns: &[ChatTurn], session_id: &str) {
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
            match crate::chat_context::write_digest(root, text, session_id) {
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
