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
use tracing::instrument;

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
    /// `--resume` で起動されたか(`session/load`、またはモデル/effort変更による
    /// 再起動)。`Message::Result{is_error:true}` の文言選びに使う
    /// ([`Self::result_error`] 参照)。
    resumed: bool,
}

impl std::fmt::Debug for ClaudeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgent")
            .field("root", &self.root)
            .finish()
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
    #[instrument]
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

        let cli_path = resolve_cli_path();
        if let Some(path) = &cli_path
            && let Some(reason) = unsupported_cli_reason(path)
        {
            return Err(reason);
        }

        let client = ClaudeSDKClient::new(options, cli_path)
            .await
            .map_err(cli_error)?;

        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            root: root.to_path_buf(),
            resumed: resume,
        })
    }

    /// 会話とは別セッションで一発だけ問い合わせる(要約用)。
    ///
    /// ツールを一切許可せず `max_turns(1)` で回すので、応答は素のテキスト1回で返る。
    /// 会話用のクライアントとは独立しているため、要約が進行中の対話へ混ざらない。
    #[instrument]
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

        let stream = query(prompt, Some(options)).await.map_err(cli_error)?;
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
                Message::Result {
                    is_error,
                    subtype,
                    errors,
                    result,
                    ..
                } => {
                    if is_error {
                        let detail = describe_result_error(&subtype, &errors, &result);
                        warn!("acp: chat digest 生成が失敗結果を返しました: {}", detail);
                        return Err(AgentError {
                            message: format!("claude reported an error result ({})", detail),
                        });
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// `Message::Result{is_error:true}` を分かりやすい `AgentError` へ変換する。
    ///
    /// `--resume` で起動したセッションが実は `claude` CLI 側に存在しない場合、
    /// `ClaudeSDKClient::new()` の接続自体は成功してしまい、`friendly_agent_error` の
    /// `Transport` 検出では捕まえられない。実際の失敗は最初の応答がこの形
    /// (`Message::Result{is_error:true}`)で返ってきたときに初めて分かる
    /// (`claude` CLI 自身は stderr に "No conversation found with session ID: ..." を
    /// 出すが、こちらのプロセスからは見えない)。`--resume` 起動だった場合は
    /// `friendly_agent_error` の `Transport` と同じ文言に寄せ、詳細はログにだけ残す。
    /// 新規セッションでの失敗は本当に別の理由(実際のAPIエラー等)の可能性があるため、
    /// 詳細をそのままユーザーへ返す。
    #[instrument]
    fn result_error(
        &self,
        subtype: &str,
        errors: &[String],
        result: &Option<String>,
    ) -> AgentError {
        let detail = describe_result_error(subtype, errors, result);
        warn!(
            "acp: claude reported an error result (resumed={}): {}",
            self.resumed, detail
        );
        if self.resumed {
            AgentError {
                message: "セッションの再開に失敗しました。新しい会話を開始してください。"
                    .to_string(),
            }
        } else {
            AgentError {
                message: format!("claude reported an error result ({})", detail),
            }
        }
    }
}

/// [`ALLOWED_TOOLS`] をビルダーが受け取る形へ変換する。
fn tool_names() -> Vec<anthropic_agent_sdk::ToolName> {
    ALLOWED_TOOLS.iter().map(|t| (*t).into()).collect()
}

/// `claude` CLI の実行ファイルパスを解決する。
///
/// SDK自身も内部で `which::which("claude")` を試すが、それに失敗した場合の
/// フォールバックが完全にPOSIX前提(`HOME`環境変数、`/usr/local/bin` 等)で、
/// `#[cfg(windows)]` 分岐が無いため Windows では実質機能しない
/// (`anthropic-agent-sdk` の `transport/subprocess.rs` の `find_cli()` 参照)。
/// ここで先に `which` を試し、それでも見つからなければ Windows のnpmグローバル
/// インストール先を明示的にチェックする。どちらも失敗したら `None` を返し、
/// SDK自身の探索(従来通りの挙動)に委ねる — 新たな失敗モードを増やさない。
#[instrument]
fn resolve_cli_path() -> Option<PathBuf> {
    if let Ok(path) = which::which("claude") {
        return Some(path);
    }
    windows_npm_fallback()
}

/// npm がWindowsでグローバルインストールする既定の場所(`%APPDATA%\npm\claude.cmd`)を
/// 明示的にチェックする。`which` がPATHから見つけられなかった場合の最後の手段。
/// `resolve_cli_path` から切り出してあるのは、`which` の成否に依らずこの部分だけを
/// 単体テストできるようにするため。
#[cfg(windows)]
#[instrument]
fn windows_npm_fallback() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let candidate = PathBuf::from(appdata).join("npm").join("claude.cmd");
    candidate.is_file().then_some(candidate)
}

#[cfg(not(windows))]
fn windows_npm_fallback() -> Option<PathBuf> {
    None
}

/// 解決した `cli_path` がそのまま起動できない形式なら理由を返す。
///
/// `npm install -g @anthropic-ai/claude-code` はWindowsでは `claude.cmd` という
/// バッチラッパーを生成する(ネイティブインストーラの `claude.exe` とは別物)。
/// `.cmd`/`.bat` を `std::process::Command` へ渡すと、Rust標準ライブラリの
/// 引数エスケープ制限(CVE-2024-24576対策。バッチファイルは `cmd.exe` 経由で
/// 実行されるため、`"`/`&`/`^` 等を含む複雑な引数を安全にエスケープできない場合に
/// 拒否する)に引っかかり、`claude` へ渡す長いシステムプロンプトが原因で
/// 「batch file arguments are invalid」という原因の分からないOSエラーになる
/// (実機で確認済み)。起動を試みる前にこの形式を検出し、分かりやすい理由で
/// 早期に失敗させる。
#[cfg(windows)]
#[instrument]
fn unsupported_cli_reason(path: &Path) -> Option<AgentError> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "cmd" && ext != "bat" {
        return None;
    }
    warn!(
        "acp: claude CLIがバッチラッパー形式で見つかりました(起動不可): {:?}",
        path
    );
    Some(AgentError {
        message: format!(
            "claude CLI が npm 経由のラッパースクリプト({})として見つかりましたが、\
             この形式は ACP 連携から起動できません(Windows の引数エスケープの制限のため)。\
             `npm uninstall -g @anthropic-ai/claude-code` のうえ、\
             Anthropic のネイティブインストーラで入れ直してください。",
            path.display()
        ),
    })
}

#[cfg(not(windows))]
fn unsupported_cli_reason(_path: &Path) -> Option<AgentError> {
    None
}

/// `ClaudeSDKClient::new`/`query` の起動失敗を分かりやすい `AgentError` へ変換する。
///
/// `ClaudeError::CliNotFound` のSDK既定メッセージは
/// `npm install -g @anthropic-ai/claude-code` の案内のあと
/// `export PATH=...` という完全にPOSIX向けの文面で、Windowsでは誤誘導になる
/// (`resolve_cli_path` 参照)。OSごとに案内し直す。
#[instrument]
fn cli_error(e: anthropic_agent_sdk::ClaudeError) -> AgentError {
    if let anthropic_agent_sdk::ClaudeError::CliNotFound(detail) = &e {
        warn!("acp: claude CLIが見つかりません: {}", detail);
        #[cfg(windows)]
        let message = "claude CLI が見つかりません。`npm install -g @anthropic-ai/claude-code` \
            でインストールしたうえで Zed を再起動してください(PATH の変更は既に起動中の \
            プロセスには反映されないため)。"
            .to_string();
        #[cfg(not(windows))]
        let message = "claude CLI が見つかりません。`npm install -g @anthropic-ai/claude-code` \
            でインストールしてください。"
            .to_string();
        return AgentError { message };
    }
    AgentError {
        message: format!("failed to start claude: {}", e),
    }
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
#[instrument]
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
#[instrument]
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
#[instrument]
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

/// `subtype`/`errors`/`result` から `Message::Result{is_error:true}` の詳細文字列を組み立てる。
///
/// `errors` が空でも `result`(成功時用のフィールドだが、実運用では失敗時にも
/// メッセージが入ることがある)があればそちらを使う。どちらも無ければ `subtype` のみ。
#[instrument]
fn describe_result_error(subtype: &str, errors: &[String], result: &Option<String>) -> String {
    if !errors.is_empty() {
        format!("{} ({})", subtype, errors.join("; "))
    } else if let Some(r) = result.as_deref().filter(|r| !r.is_empty()) {
        format!("{} ({})", subtype, r)
    } else {
        subtype.to_string()
    }
}

/// 応答ブロックからテキストだけを取り出して連結する。
#[instrument]
fn append_text(out: &mut String, blocks: &[ContentBlock]) {
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
}

#[async_trait::async_trait]
impl WritingAgent for ClaudeAgent {
    #[instrument(skip(self, on_chunk))]
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
                    is_error,
                    subtype,
                    errors,
                    result,
                    ..
                } => {
                    if is_error {
                        return Err(self.result_error(&subtype, &errors, &result));
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

    // #[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
    #[instrument]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `APPDATA` は process-wide なので、テスト同士が並行実行されても
    /// 競合しないよう1本の Mutex で直列化する(`main.rs` の `RUST_LOG` テストと同じ方針)。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    #[cfg(windows)]
    fn test_windows_npm_fallback_finds_claude_cmd() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("ff_writing_agent_appdata_hit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("npm")).unwrap();
        std::fs::write(dir.join("npm").join("claude.cmd"), "@echo off\n").unwrap();

        let original = std::env::var_os("APPDATA");
        unsafe { std::env::set_var("APPDATA", &dir) };

        let found = windows_npm_fallback();

        match original {
            Some(v) => unsafe { std::env::set_var("APPDATA", v) },
            None => unsafe { std::env::remove_var("APPDATA") },
        }

        assert_eq!(found, Some(dir.join("npm").join("claude.cmd")));
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_npm_fallback_returns_none_when_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("ff_writing_agent_appdata_miss");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // `npm/claude.cmd` を作らない。

        let original = std::env::var_os("APPDATA");
        unsafe { std::env::set_var("APPDATA", &dir) };

        let found = windows_npm_fallback();

        match original {
            Some(v) => unsafe { std::env::set_var("APPDATA", v) },
            None => unsafe { std::env::remove_var("APPDATA") },
        }

        assert_eq!(found, None);
    }

    #[test]
    #[cfg(not(windows))]
    fn test_windows_npm_fallback_is_noop_on_non_windows() {
        assert_eq!(windows_npm_fallback(), None);
    }

    #[test]
    #[cfg(windows)]
    fn test_unsupported_cli_reason_rejects_cmd_and_bat() {
        let err = unsupported_cli_reason(Path::new(r"C:\npm\claude.cmd")).unwrap();
        assert!(err.message.contains("ネイティブインストーラ"));
        let err = unsupported_cli_reason(Path::new(r"C:\npm\claude.BAT")).unwrap();
        assert!(err.message.contains("ネイティブインストーラ"));
    }

    #[test]
    #[cfg(windows)]
    fn test_unsupported_cli_reason_accepts_exe_and_extensionless() {
        assert!(unsupported_cli_reason(Path::new(r"C:\bin\claude.exe")).is_none());
        assert!(unsupported_cli_reason(Path::new(r"C:\bin\claude")).is_none());
    }

    #[test]
    #[cfg(not(windows))]
    fn test_unsupported_cli_reason_is_noop_on_non_windows() {
        assert!(unsupported_cli_reason(Path::new("/usr/bin/claude.cmd")).is_none());
    }

    #[test]
    fn test_cli_error_maps_cli_not_found_to_actionable_message() {
        let e = anthropic_agent_sdk::ClaudeError::CliNotFound("raw sdk message".to_string());
        let err = cli_error(e);
        assert!(
            err.message
                .contains("npm install -g @anthropic-ai/claude-code")
        );
        // SDK既定の(POSIX前提の)文面は出さない。
        assert!(!err.message.contains("export PATH"));
    }

    #[test]
    fn test_cli_error_passes_through_other_errors() {
        let e = anthropic_agent_sdk::ClaudeError::Transport("pipe closed".to_string());
        let err = cli_error(e);
        assert!(err.message.contains("failed to start claude"));
        assert!(err.message.contains("pipe closed"));
    }

    #[test]
    fn test_describe_result_error_prefers_errors_over_result() {
        let s = describe_result_error(
            "error_during_execution",
            &["boom".to_string()],
            &Some("fallback".to_string()),
        );
        assert_eq!(s, "error_during_execution (boom)");
    }

    #[test]
    fn test_describe_result_error_falls_back_to_result_then_subtype() {
        let s = describe_result_error("error_during_execution", &[], &Some("fallback".to_string()));
        assert_eq!(s, "error_during_execution (fallback)");

        let s = describe_result_error("error_during_execution", &[], &None);
        assert_eq!(s, "error_during_execution");
    }
}
