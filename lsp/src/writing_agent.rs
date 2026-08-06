//! 執筆相談エージェントへの入口。
//!
//! Claude Agent SDK(`anthropic-agent-sdk` クレート)への依存を**このモジュールだけ**に
//! 閉じ込める。`crate::acp` は [`WritingAgent`] トレイト越しにしか触らないので、
//! クレートを直接 `claude` CLI 駆動へ差し替えるときの影響範囲がここで止まる。
//!
//! # クレートについて
//!
//! `anthropic-agent-sdk` は Anthropic 公式ではなく第三者製(MIT)。公式の Agent SDK は
//! Python と TypeScript のみで、他言語には「`claude` CLI をサブプロセスで駆動せよ」と
//! 案内されている。このクレートも中身は同じで CLI のラッパーなので、いざとなれば
//! 同じフラグを自前で組み立てる実装に置き換えられる(下記 `CLI フラグ`参照)。
//!
//! # CLAUDE.md を一切読ませない
//!
//! 執筆用エージェントに、コーディング向けの CLAUDE.md が混ざると邪魔になる。
//! `setting_sources` を **設定しない**ことで、クレートは `--setting-sources ""` を渡す。
//! これによりプロジェクトの `CLAUDE.md` も `~/.claude/CLAUDE.md` も output styles も
//! `settings.json` も一切読み込まれない。加えて [`SystemPrompt::String`] を使うので
//! `--system-prompt` となり、Claude Code の既定プロンプトも丸ごと置き換わる。
//!
//! # CLI フラグ
//!
//! このモジュールの設定が最終的に `claude` へ渡す形:
//!
//! ```text
//! --system-prompt <data/system_chat.md の中身>
//! --setting-sources ""
//! --allowedTools Read,Write,Edit,Glob,Grep,WebSearch,WebFetch
//! ```

use crate::acp_config::SessionConfig;
use anthropic_agent_sdk::{
    ClaudeAgentOptions, ClaudeSDKClient, ContentBlock, Message, PermissionMode, StreamExt, query,
};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::path::{Path, PathBuf};

/// エージェントに許可するツール。
///
/// 執筆支援に必要なものだけを明示的に挙げる。**`Bash` は入れない** —
/// 原稿ディレクトリで任意のコマンドを実行できる必要はなく、許可範囲は狭いほどよい。
/// `Read`/`Write`/`Edit`/`Glob`/`Grep` があれば characters.md・plot.md・memo/*.md の
/// 読み書きは足りるし、`WebSearch`/`WebFetch` で調べ物もできる。
const ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "WebSearch",
    "WebFetch",
];

/// エージェント操作の失敗。
///
/// SDK/CLI 側の失敗だけを表す。プロンプトが読めない等のこちら側の設定不備は
/// ACP の起動時([`crate::acp::run`])に弾くので、ここには来ない。
#[derive(Debug, derive_more::Display)]
#[display("agent failed: {}", message)]
pub(crate) struct AgentError {
    pub(crate) message: String,
}

impl std::error::Error for AgentError {}

/// サブスクリプション枠(5時間枠)の状況。`claude` CLI が流す `rate_limit_event` から拾う。
///
/// SDK にも ACP にも対応する型が無いので、こちらで生JSONから最小限だけ読む
/// (詳細は [`parse_rate_limit_event`] 参照)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RateLimit {
    /// 枠の使用率 (0.0..=100.0)。
    pub(crate) utilization: f64,
    /// 枠がリセットされる時刻(表示用。取れなければ `None`)。
    pub(crate) resets_at: Option<String>,
}

/// 1ターンの応答。
pub(crate) struct TurnReply {
    /// 応答テキストの全文(要約の材料)。
    pub(crate) text: String,
    /// このターン中に `rate_limit_event` が来ていれば、最後(=最新)の1件。
    pub(crate) rate_limit: Option<RateLimit>,
}

/// 作者と対話する執筆相談エージェント。
///
/// 実装は [`ClaudeAgent`]。テストでは差し替えられるようトレイトにしてある。
#[async_trait::async_trait]
pub(crate) trait WritingAgent: Send + Sync + std::fmt::Debug {
    /// 1ターン投げて応答を得る。
    ///
    /// 応答テキストは届いた順に `on_chunk` へ渡す(ACP の `AgentMessageChunk` へ流すため)。
    async fn prompt(
        &self,
        text: &str,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<TurnReply, AgentError>;

    /// 進行中のターンを中断する(ACP の `session/cancel` 用)。
    async fn interrupt(&self) -> Result<(), AgentError>;
}

/// Claude Agent SDK を使う実装。
///
/// `ClaudeSDKClient` は送受信に `&mut self` を要求するため Mutex で包む。
/// 1セッション＝1クライアント＝1つの CLI プロセスで、会話の文脈は CLI 側が保持する
/// (このモジュールが履歴を組み立て直す必要はない)。
pub(crate) struct ClaudeAgent {
    client: tokio::sync::Mutex<ClaudeSDKClient>,
    /// 要約の一発問い合わせで使い回すワークスペースルート。
    root: PathBuf,
}

impl std::fmt::Debug for ClaudeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgent").field("root", &self.root).finish()
    }
}

impl ClaudeAgent {
    /// ワークスペース `root` に紐づくエージェントを起動する。
    ///
    /// `system_prompt` は Claude Code の既定プロンプトを**置き換える**(追記ではない)。
    ///
    /// `session_id` は `claude` CLI 自身のセッションID(UUID形式)。`resume=false` なら
    /// `--session-id` でこのIDを新規セッションに割り当て、`resume=true` なら
    /// `--resume` でこのIDの既存セッション(CLI側がディスクに永続化している)を再開する。
    /// ACP の `session/new`/`session/load` それぞれに対応する
    /// (`crate::acp` はこのIDをそのまま ACP の `SessionId` として使い回すため、
    /// 呼び出し側で別途IDのマッピングを持つ必要がない)。
    pub(crate) async fn start(
        root: &Path,
        system_prompt: String,
        session_id: &str,
        resume: bool,
        config: &SessionConfig,
    ) -> Result<Self, AgentError> {
        // `setting_sources` をあえて設定しない — これがクレート側で
        // `--setting-sources ""` になり CLAUDE.md 類を全て締め出す(モジュール冒頭参照)。
        let mut options = if resume {
            ClaudeAgentOptions::builder()
                .system_prompt(system_prompt)
                .cwd(root.to_path_buf())
                .permission_mode(PermissionMode::AcceptEdits)
                .allowed_tools(tool_names())
                .resume(session_id)
                .build()
        } else {
            ClaudeAgentOptions::builder()
                .system_prompt(system_prompt)
                .cwd(root.to_path_buf())
                .permission_mode(PermissionMode::AcceptEdits)
                .allowed_tools(tool_names())
                .session_id(session_id)
                .build()
        };
        // `TypedBuilder` は条件付き呼び出しができないので、`None` なら CLI 任せの既定を
        // 保つべく素通りさせ、`Some` のときだけ組み立て済みの options に直接代入する。
        if let Some(model) = &config.model {
            options.model = Some(model.clone());
        }
        if let Some(tokens) = config.thinking_tokens {
            options.max_thinking_tokens = Some(tokens);
        }

        let client = ClaudeSDKClient::new(options, None)
            .await
            .map_err(|e| AgentError {
                message: format!("failed to start claude: {}", e),
            })?;

        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            root: root.to_path_buf(),
        })
    }

    /// 会話とは別セッションで一発だけ問い合わせる(要約用)。
    ///
    /// ツールを一切許可せず `max_turns(1)` で回すので、応答は素のテキスト1回で返る。
    /// 会話用のクライアントとは独立しているため、要約が進行中の対話へ混ざらない。
    pub(crate) async fn one_shot(
        root: &Path,
        system_prompt: String,
        prompt: String,
    ) -> Result<String, AgentError> {
        let options = ClaudeAgentOptions::builder()
            .system_prompt(system_prompt)
            .cwd(root.to_path_buf())
            .disallowed_tools(tool_names())
            .max_turns(1)
            .build();

        let stream = query(prompt, Some(options)).await.map_err(|e| AgentError {
            message: format!("failed to start claude: {}", e),
        })?;
        let mut stream = Box::pin(stream);

        let mut out = String::new();
        while let Some(message) = stream.next().await {
            let message = match parse_line(message)? {
                ParsedLine::Message(m) => m,
                // 要約用の一発問い合わせでは枠の使用率を見せる先が無いので単に読み捨てる。
                ParsedLine::RateLimit(_) | ParsedLine::Skip => continue,
            };
            match message {
                Message::Assistant { message, .. } => append_text(&mut out, &message.content),
                Message::Result { is_error, .. } => {
                    if is_error {
                        return Err(AgentError {
                            message: "claude reported an error result".to_string(),
                        });
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// [`ALLOWED_TOOLS`] をビルダーが受け取る形へ変換する。
fn tool_names() -> Vec<anthropic_agent_sdk::ToolName> {
    ALLOWED_TOOLS.iter().map(|t| (*t).into()).collect()
}

/// 1行分のパース結果。
enum ParsedLine {
    /// SDKが正しく型付けできたメッセージ。
    Message(Message),
    /// SDKがまだ知らない `rate_limit_event`。中身を読めた場合。
    RateLimit(RateLimit),
    /// SDKがまだ知らない、かつ枠の情報でもないメッセージ種別。読み捨てる。
    Skip,
}

/// `claude` CLI が送ってくる、SDK側がまだ知らないメッセージ種別による
/// `ClaudeError::MessageParse` を無視して読み進める。
///
/// `rate_limit_event` だけは [`parse_rate_limit_event`] で中身を拾う
/// (SDK にも ACP にも対応する型が無いため、`ClaudeError::MessageParse` が
/// 保持する生JSONから直接読む)。それ以外の未知種別は今まで通り警告1行で捨てる。
/// SDK が型付けに失敗した以外のエラーはそのまま `AgentError` として伝播させる。
fn parse_line(message: anthropic_agent_sdk::Result<Message>) -> Result<ParsedLine, AgentError> {
    match message {
        Ok(m) => Ok(ParsedLine::Message(m)),
        Err(anthropic_agent_sdk::ClaudeError::MessageParse { message, data }) => {
            if let Some(rate_limit) = data.as_ref().and_then(parse_rate_limit_event) {
                return Ok(ParsedLine::RateLimit(rate_limit));
            }
            debug!("unparseable message raw: {:?}", data);
            warn!(
                "acp: claude CLIからの未知のメッセージ種別を無視します: {}",
                message
            );
            Ok(ParsedLine::Skip)
        }
        Err(e) => Err(friendly_agent_error(e)),
    }
}

/// `rate_limit_event` の生JSONから枠の使用率を読む。
///
/// SDK にも ACP にもこの種別の型が無いため、`type` フィールドで判定したうえで
/// `rateLimit`/直下のどちらにキーがあっても拾えるようにしてある。想定した形で
/// なければ `None` を返して黙って何もしない(要約の欠落と同じく、取れなくても
/// 会話自体は成立するため)。
fn parse_rate_limit_event(data: &serde_json::Value) -> Option<RateLimit> {
    if data.get("type").and_then(|t| t.as_str()) != Some("rate_limit_event") {
        return None;
    }
    let payload = data.get("rateLimit").unwrap_or(data);

    let utilization = payload
        .get("utilization")
        .or_else(|| payload.get("percentUsed"))
        .and_then(serde_json::Value::as_f64)?;
    let resets_at = payload
        .get("resetsAt")
        .or_else(|| payload.get("resets_at"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Some(RateLimit {
        utilization,
        resets_at,
    })
}

/// SDKのエラーを、ユーザー向けに分かりやすい `AgentError` へ変換する。
///
/// `Transport` エラー(「パイプが閉じている」等の生のOSエラーを含む)は、
/// `--resume` で指定したセッションが `claude` CLI 側に存在しない・起動直後に
/// 引数エラーで終了した等の理由で接続が断たれたことを示す。生のOSエラー文言のままだと
/// 原因が伝わらないため、分かりやすい文言に置き換える
/// (自動フォールバックはせず、ユーザーに新しい会話の開始を促すだけに留める)。
fn friendly_agent_error(e: anthropic_agent_sdk::ClaudeError) -> AgentError {
    match e {
        anthropic_agent_sdk::ClaudeError::Transport(detail) => {
            warn!("acp: transport error (session不整合の可能性): {}", detail);
            AgentError {
                message: "セッションの再開に失敗しました。新しい会話を開始してください。"
                    .to_string(),
            }
        }
        other => AgentError {
            message: other.to_string(),
        },
    }
}

/// 応答ブロックからテキストだけを取り出して連結する。
fn append_text(out: &mut String, blocks: &[ContentBlock]) {
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
}

#[async_trait::async_trait]
impl WritingAgent for ClaudeAgent {
    async fn prompt(
        &self,
        text: &str,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<TurnReply, AgentError> {
        let mut client = self.client.lock().await;

        client
            .send_message(text.to_string())
            .await
            .map_err(friendly_agent_error)?;

        let mut full = String::new();
        let mut rate_limit: Option<RateLimit> = None;
        while let Some(message) = client.next_message().await {
            let message = match parse_line(message)? {
                ParsedLine::Message(m) => m,
                ParsedLine::RateLimit(r) => {
                    // 複数回来たら最後(=最新)の1件を採る。
                    rate_limit = Some(r);
                    continue;
                }
                ParsedLine::Skip => continue,
            };
            match message {
                Message::Assistant { message, .. } => {
                    // 届いた分をその場でクライアントへ流しつつ、要約用に全文も貯める。
                    let mut piece = String::new();
                    append_text(&mut piece, &message.content);
                    if !piece.is_empty() {
                        full.push_str(&piece);
                        on_chunk(piece);
                    }
                }
                Message::Result {
                    is_error, subtype, ..
                } => {
                    if is_error {
                        return Err(AgentError {
                            message: format!("claude reported an error result ({})", subtype),
                        });
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(TurnReply {
            text: full,
            rate_limit,
        })
    }

    async fn interrupt(&self) -> Result<(), AgentError> {
        self.client
            .lock()
            .await
            .interrupt()
            .await
            .map_err(|e| AgentError {
                message: e.to_string(),
            })
    }
}
