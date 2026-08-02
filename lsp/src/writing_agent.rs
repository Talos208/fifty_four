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

/// 作者と対話する執筆相談エージェント。
///
/// 実装は [`ClaudeAgent`]。テストでは差し替えられるようトレイトにしてある。
#[async_trait::async_trait]
pub(crate) trait WritingAgent: Send + Sync + std::fmt::Debug {
    /// 1ターン投げて応答を得る。
    ///
    /// 応答テキストは届いた順に `on_chunk` へ渡す(ACP の `AgentMessageChunk` へ流すため)。
    /// 返り値は同じテキストの全文で、要約の材料に使う。
    async fn prompt(
        &self,
        text: &str,
        on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<String, AgentError>;

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
    pub(crate) async fn start(root: &Path, system_prompt: String) -> Result<Self, AgentError> {
        // `setting_sources` をあえて設定しない — これがクレート側で
        // `--setting-sources ""` になり CLAUDE.md 類を全て締め出す(モジュール冒頭参照)。
        let options = ClaudeAgentOptions::builder()
            .system_prompt(system_prompt)
            .cwd(root.to_path_buf())
            .permission_mode(PermissionMode::AcceptEdits)
            .allowed_tools(tool_names())
            .build();

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
            match message.map_err(|e| AgentError {
                message: e.to_string(),
            })? {
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
    ) -> Result<String, AgentError> {
        let mut client = self.client.lock().await;

        client
            .send_message(text.to_string())
            .await
            .map_err(|e| AgentError {
                message: e.to_string(),
            })?;

        let mut full = String::new();
        while let Some(message) = client.next_message().await {
            match message.map_err(|e| AgentError {
                message: e.to_string(),
            })? {
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
        Ok(full)
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
