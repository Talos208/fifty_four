//! ACP (Agent Client Protocol) エージェント。`fifty_four_lsp --acp` で起動したときのモード。
//!
//! Zed の Agent Panel から stdio 越しに接続され、作者の相談相手として応答する
//! (中身は [`crate::writing_agent`] 経由の Claude Agent SDK)。目的はチャットそのもの
//! ではなく、**会話の要約を [`crate::chat_context`] へ書き出し、LSP の補完・code action
//! プロンプトへ `{{CHAT}}` として渡す**こと。
//!
//! 構成・認証(会話本体は `claude` CLI のサブスク枠、要約は別 provider で
//! `scrub_anthropic_credentials` により資格情報混線を防いでいる)の詳細は
//! `docs/acp-agent.md` 参照。

use crate::acp_config::{self, SessionConfig};
use crate::writing_agent::{AgentError, ClaudeAgent, WritingAgent};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthMethodTerminal, AuthenticateRequest,
    AuthenticateResponse, CancelNotification, ClientCapabilities, ContentBlock, ContentChunk,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse, Meta,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    /*SessionCapabilities,*/ SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, UsageUpdate,
};
use agent_client_protocol::{Agent, Stdio};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::instrument;

/// 要約に渡す過去ターン数の上限。
///
/// 拾うべきなのは「いま書こうとしている場面」なので、会話全体を渡す必要はない。
const MAX_DIGEST_TURNS: usize = 8;

/// 接続が切れたあと、走っている要約タスクを待つ上限。
///
/// 要約は `session/prompt` の応答を返したあとに走るので、その直後に Zed が切断すると
/// 書き終える前にランタイムごと落ちる。ここで待つことで取りこぼしを防ぐ。
const DIGEST_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 「Claude にログイン」の認証方法ID(ACP の `authMethods` に載せる)。
const LOGIN_METHOD_ID: &str = "claude-login";

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
#[derive(Debug)]
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
    /// このセッションの `claude` プロセスが一度でも応答を完了したか。
    ///
    /// `false` のうちは `claude` CLI 側にそのセッションIDの会話記録が
    /// まだ存在しない(`session/new` で起こしたプロセスがまだ一度も
    /// ターンを終えていない)。設定変更の再起動(`session/prompt` 冒頭)で
    /// このときに `--resume` してしまうと、記録の無いIDを再開しようとして
    /// 「No conversation found」で失敗する。`session/load` は再開対象として
    /// 読み込む以上ここに会話がある前提で `true` から始める。
    has_replied: bool,
}

/// ハンドラ間で共有する状態。
#[derive(Debug)]
struct AgentState {
    sessions: tokio::sync::Mutex<HashMap<SessionId, Session>>,
    /// 実行中の要約タスク。切断時に待ち合わせるため保持する([`DIGEST_DRAIN_TIMEOUT`])。
    digests: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    /// 要約(chat digest)専用の LLM。`crate::llm::build_client` で1度だけ組み立てて使い回す
    /// (`crate::llm::use_llm_with_option` が要求する形。LSP 側の `Backend::llm` と同じ持ち方)。
    /// 組み立てに失敗した場合は `None` のままにし、[`update_digest`] が
    /// `LlmError::NotInitialized` で警告を出して終わる(要約が出ないだけで会話は成立する)。
    digest_llm: tokio::sync::Mutex<Option<Box<dyn crate::llm::LlmInterface>>>,
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

/// `initialize` で返す認証方法。`claude` CLI のログインが切れたとき、Zed の
/// Agent Panel に「ログイン」ボタンを出させるためのもの(`auth_required` エラーと対)。
///
/// どちらの形式でも、Zed は内蔵ターミナルで `fifty_four_lsp --acp --login` を走らせ、
/// それが `claude auth login` を実行する(`main.rs`/[`crate::writing_agent::run_login`])。
/// - 現行仕様: クライアントが `auth.terminal` を宣言していれば `type: "terminal"`。
///   クライアントは設定済みのエージェント起動コマンド(`--acp` 付き)に `args` を足して実行する。
/// - 旧形式: `_meta["terminal-auth"]` を宣言するクライアント(古めの Zed)向けに、
///   実行するコマンドを `_meta["terminal-auth"]` で丸ごと渡す。
///
/// どちらも宣言していないクライアントには何も返さない(仕様上、端末を開けない
/// クライアントへ terminal 方式を載せてはいけないため)。
fn auth_methods(caps: &ClientCapabilities) -> Vec<AuthMethod> {
    const NAME: &str = "Claude にログイン";
    const DESCRIPTION: &str =
        "claude CLI のログインが切れたとき、`claude auth login` でログインし直します";

    if caps.auth.terminal {
        return vec![AuthMethod::Terminal(
            AuthMethodTerminal::new(LOGIN_METHOD_ID, NAME)
                .description(DESCRIPTION.to_string())
                .args(vec!["--login".to_string()]),
        )];
    }

    let legacy = caps
        .meta
        .as_ref()
        .and_then(|m| m.get("terminal-auth"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !legacy {
        return Vec::new();
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            warn!(
                "acp: 実行ファイルのパスが取れないためログイン方法を提示しません: {}",
                e
            );
            return Vec::new();
        }
    };
    let mut meta = Meta::new();
    meta.insert(
        "terminal-auth".to_string(),
        serde_json::json!({
            "label": "claude auth login",
            "command": exe.to_string_lossy(),
            "args": ["--acp", "--login"],
        }),
    );
    vec![AuthMethod::Agent(
        AuthMethodAgent::new(LOGIN_METHOD_ID, NAME)
            .description(DESCRIPTION.to_string())
            .meta(meta),
    )]
}

/// ACP エージェントとして stdio で待ち受ける。
#[instrument]
pub(crate) async fn run() -> Result<(), String> {
    // 起動時に読めないと全セッションが失敗するので、ここで確かめておく。
    let _ = system_prompt()?;

    let state = Arc::new(AgentState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        digests: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
        digest_llm: tokio::sync::Mutex::new(Some(
            crate::llm::build_client_async(&digest_llm_config(), "system_chat_digest.md").await,
        )),
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
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(
                            // `claude` CLI の `--session-id`/`--resume` がそのまま使えるため
                            // `session/load` に対応できる(下のハンドラ参照)。
                            AgentCapabilities::new().load_session(true),
                        )
                        .auth_methods(auth_methods(&req.client_capabilities)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `authenticate`: ログイン自体は Zed が端末で `--acp --login` を走らせて済ませる
        // ([`auth_methods`] 参照)ので、ここでは何もせず成功を返すだけ。
        // 旧形式(`_meta.terminal-auth`)のクライアントが端末での実行後に呼んでくる場合がある。
        .on_receive_request(
            async move |req: AuthenticateRequest, responder, _cx| {
                debug!("acp authenticate: method_id={:?}", req.method_id);
                responder.respond(AuthenticateResponse::default())
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
                        has_replied: false,
                    },
                );
                responder.respond(NewSessionResponse::new(id).config_options(config_options))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // `session/load`: セッションを CLI 側の永続化履歴から再開する。
        // 詳細(過去ターンのリプレイ、旧形式ID非対応の理由)は `docs/acp-agent.md` の「セッションの再開」参照
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, connection| {
                debug!("acp session/load: id={} cwd={:?}", req.session_id, req.cwd);

                // 旧形式ID(UUID以外)は `--resume` に渡すと分かりにくく失敗するため、ここで弾く。
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
                        let digest_state = load_session.clone();
                        let mut digests = load_session.digests.lock().await;
                        while digests.try_join_next().is_some() {}
                        digests.spawn(async move {
                            update_digest(
                                &root,
                                &turns_for_digest,
                                &session_id,
                                &digest_state.digest_llm,
                            )
                            .await;
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
                        has_replied: true,
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
                            Some((
                                s.root.clone(),
                                s.agent.clone(),
                                s.pending.clone(),
                                s.has_replied,
                            ))
                        }
                        None => None,
                    }
                };

                let Some((root, mut agent, pending, has_replied)) = session else {
                    warn!("acp session/prompt: unknown session {}", session_id);
                    return responder
                        .respond_with_internal_error(format!("unknown session: {}", session_id));
                };

                // モデル/思考レベル切替はセッション途中は非対応なので、ここ(会話の切れ目)で `claude` プロセスを起こし直す。
                // `has_replied` で `--resume`/`--session-id` を切り分ける理由は `docs/acp-agent.md` の「モデル・思考レベルの変更」参照
                if let Some(new_config) = pending {
                    let prompt = match system_prompt() {
                        Ok(p) => p,
                        Err(e) => return responder.respond_with_internal_error(e),
                    };
                    match ClaudeAgent::start(&root, prompt, &session_id.0, has_replied, &new_config)
                        .await
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
                    Err(AgentError {
                        message,
                        auth_required: true,
                    }) => {
                        // いま動いている `claude` プロセスは古い資格情報を握ったままなので、
                        // ログインし直しても次のターンで失敗し続ける。設定変更と同じ経路で、
                        // 次の `session/prompt` の頭でプロセスを起こし直させる
                        // (ログイン後の資格情報は起動時に読み直される)。
                        warn!("acp prompt failed (auth required): {}", message);
                        if let Some(s) = prompt_state.sessions.lock().await.get_mut(&session_id) {
                            s.pending = Some(s.pending.clone().unwrap_or_else(|| s.config.clone()));
                        }
                        let mut err = agent_client_protocol::Error::auth_required();
                        err.message = message;
                        return responder.respond_with_error(err);
                    }
                    Err(AgentError { message, .. }) => {
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
                            // ここに来た時点で agent.prompt() は成功している
                            // (エラーは手前で早期returnしている)。つまり
                            // `claude` CLI 側にこのセッションIDの会話記録ができた。
                            s.has_replied = true;
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
                    let digest_state = prompt_state.clone();
                    let mut digests = prompt_state.digests.lock().await;
                    // 完了済みを回収してから積む(放っておくと JoinSet が伸び続ける)。
                    while digests.try_join_next().is_some() {}
                    let owner_id = session_id.0.to_string();
                    digests.spawn(async move {
                        update_digest(&root, &turns, &owner_id, &digest_state.digest_llm).await;
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
#[instrument]
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

/// 要約(chat digest)用 LLM の provider/model 設定。`FIFTY_FOUR_ACP_LLM` 環境変数の
/// 形式・既定値は `docs/acp-agent.md` の「プロンプト」参照。
///
/// `LlmClientBuilder::from_value` は不正な `provider` を `unwrap()` で panic させるため、
/// **ここで必ず検証してから返す**。
#[instrument(ret)]
fn digest_llm_config() -> serde_json::Value {
    const DEFAULT: &str = r#"{"provider": "google"}"#;
    let raw = std::env::var("FIFTY_FOUR_ACP_LLM").ok();
    let Some(raw) = raw else {
        return serde_json::from_str(DEFAULT).expect("DEFAULT is valid JSON");
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => match value.get("provider").and_then(|v| v.as_str()) {
            Some(provider) if crate::llm::Provider::from_str(provider).is_ok() => value,
            _ => {
                warn!(
                    "FIFTY_FOUR_ACP_LLM: provider が不正または未指定です({:?})。既定(Gemini)を使います",
                    raw
                );
                serde_json::from_str(DEFAULT).expect("DEFAULT is valid JSON")
            }
        },
        Err(e) => {
            warn!(
                "FIFTY_FOUR_ACP_LLM: JSON として解釈できません({}: {:?})。既定(Gemini)を使います",
                e, raw
            );
            serde_json::from_str(DEFAULT).expect("DEFAULT is valid JSON")
        }
    }
}

/// 会話用のシステムプロンプトを読む。
///
/// Claude Code の既定プロンプトを**置き換える**中身なので、執筆支援としての役割・
/// 原稿ディレクトリの約束事(characters.md / plot.md / memo/*.md)・要約ファイルの
/// 維持義務は全てここに書いてある。
#[instrument]
fn system_prompt() -> Result<String, String> {
    crate::assets::load("system_chat.md")
        .ok_or_else(|| "system_chat.md not found on disk nor in embedded assets".to_string())
}

/// `PromptRequest` からテキストだけを取り出して連結する。
#[instrument]
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
#[instrument(skip(turns))]
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
/// 会話本体(`claude` CLI、サブスク枠)とは別に、`crate::llm` 経由の LLM(既定は
/// Gemini。[`digest_llm_config`] 参照)で処理する。会話側のクライアントを占有しないので
/// 要約中でも次のターンを受けられる、という性質は変わらない。失敗はログに落とすだけで
/// 握りつぶす — 要約が無くても補完は `{{CHAT}}` が空になるだけで動く。
#[instrument(skip(turns, digest_llm))]
async fn update_digest(
    root: &std::path::Path,
    turns: &[ChatTurn],
    session_id: &str,
    digest_llm: &tokio::sync::Mutex<Option<Box<dyn crate::llm::LlmInterface>>>,
) {
    let Some((template, options)) = crate::frontmatter::load_prompt("prompt_chat_digest.md") else {
        warn!("prompt_chat_digest.md not found; chat context will not be updated");
        return;
    };

    let history = render_history(turns, MAX_DIGEST_TURNS);
    let vars = HashMap::from([("HISTORY", history.as_str())]);
    let prompt = crate::frontmatter::expand(&template, &vars);

    let result = crate::llm::use_llm_with_option(digest_llm, options, async |l| {
        l.add(crate::llm::Content::Text(prompt));
        l.chat().await
    })
    .await;

    match result {
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
    use crate::test_util::EnvGuard;
    use agent_client_protocol::schema::v1::TextContent;

    #[test]
    fn test_auth_methods_uses_terminal_type_when_client_supports_it() {
        let caps: ClientCapabilities =
            serde_json::from_value(serde_json::json!({ "auth": { "terminal": true } })).unwrap();
        let json = serde_json::to_value(auth_methods(&caps)).unwrap();
        assert_eq!(json[0]["type"], "terminal");
        assert_eq!(json[0]["id"], LOGIN_METHOD_ID);
        assert_eq!(json[0]["args"], serde_json::json!(["--login"]));
    }

    #[test]
    fn test_auth_methods_falls_back_to_legacy_terminal_auth_meta() {
        let caps: ClientCapabilities =
            serde_json::from_value(serde_json::json!({ "_meta": { "terminal-auth": true } }))
                .unwrap();
        let json = serde_json::to_value(auth_methods(&caps)).unwrap();
        let meta = &json[0]["_meta"]["terminal-auth"];
        assert_eq!(meta["args"], serde_json::json!(["--acp", "--login"]));
        assert!(meta["command"].as_str().is_some_and(|c| !c.is_empty()));
    }

    #[test]
    fn test_auth_methods_is_empty_without_terminal_support() {
        assert!(auth_methods(&ClientCapabilities::default()).is_empty());
    }

    /// `FIFTY_FOUR_ACP_LLM` はプロセス全体の環境変数なので、テストが並行に
    /// 走ると互いの `set_var`/`remove_var` が競合する
    /// (`crate::RUST_LOG_TEST_LOCK` と同じ理由。あちらは `RUST_LOG` 専用なので
    /// 別変数を扱うここでは流用せず、専用のロックを用意する)。
    static ACP_LLM_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_digest_llm_config_defaults_to_gemini_when_unset() {
        let _guard = ACP_LLM_ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::unset("FIFTY_FOUR_ACP_LLM");

        let cfg = digest_llm_config();

        assert_eq!(cfg["provider"], "google");
    }

    #[test]
    fn test_digest_llm_config_uses_valid_override() {
        let _guard = ACP_LLM_ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set(
            "FIFTY_FOUR_ACP_LLM",
            r#"{"provider": "xai", "model": "grok-4.5"}"#,
        );

        let cfg = digest_llm_config();

        assert_eq!(cfg["provider"], "xai");
        assert_eq!(cfg["model"], "grok-4.5");
    }

    #[test]
    fn test_digest_llm_config_falls_back_on_invalid_json() {
        let _guard = ACP_LLM_ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FIFTY_FOUR_ACP_LLM", "{not json");

        let cfg = digest_llm_config();

        assert_eq!(cfg["provider"], "google");
    }

    #[test]
    fn test_digest_llm_config_falls_back_on_unknown_provider() {
        let _guard = ACP_LLM_ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FIFTY_FOUR_ACP_LLM", r#"{"provider": "no-such-provider"}"#);

        let cfg = digest_llm_config();

        assert_eq!(cfg["provider"], "google");
    }

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
