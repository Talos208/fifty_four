//! `claude` CLI に「いま選べるモデルの一覧」を問い合わせる。
//!
//! `anthropic-agent-sdk` の `ClaudeSDKClient::supported_models()` は SDK に埋め込まれた
//! 静的リストで、SDK のリリースが止まっているため古くなる一方である。一方 `claude` CLI は
//! stream-json モードで `initialize` の control request を受けると、応答の `models` に
//! そのアカウント・バージョンで選べるモデル(`claude-fable-5-1[1m]` のような1M版を含む)を
//! 返す(TS 版 Agent SDK の `supportedModels()` もこれを読んでいる)。
//! SDK はこの request を送らず応答も捨てるので、ここで短命の `claude` プロセスを
//! 別に起こして直接問い合わせる。プロンプトは送らないので課金は発生しない。
//!
//! 応答の JSON 形式は SDK にも ACP にも型定義が無いため、必要なフィールドだけを緩く読む
//! (`writing_agent.rs` の `parse_rate_limit_event()` と同じ方針)。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, instrument, warn};

/// 問い合わせ全体(起動〜応答)の制限時間。
/// 実測では SessionStart hook の実行込みで1秒程度。hook が重い環境を見込んで余裕を持たせる。
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// `initialize` の control request に付ける ID。応答の取り違えを防ぐために照合する。
const REQUEST_ID: &str = "fifty_four_models";

/// `initialize` 応答の `models` の1件。使うフィールドだけを持つ。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CliModel {
    /// `--model` にそのまま渡せる値(`default` / `opus` / `claude-fable-5-1[1m]` など)。
    pub(crate) value: String,
    #[serde(default, rename = "displayName")]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// effort を指定できるか。`false` のモデルには effort の選択肢を出さない。
    #[serde(default, rename = "supportsEffort")]
    pub(crate) supports_effort: Option<bool>,
    /// 指定できる effort の段階(`low` / `medium` / `high` / `xhigh` / `max` など)。
    #[serde(default, rename = "supportedEffortLevels")]
    pub(crate) supported_effort_levels: Vec<String>,
}

/// `cwd` をカレントにして `claude` CLI を起こし、選べるモデルの一覧を取得する。
///
/// `--setting-sources project` は本体のエージェント(`writing_agent.rs`)と揃えてある
/// (プロジェクト設定がモデルの選択肢に影響する場合に、表示と実際の起動条件を一致させるため)。
/// `--no-session-persistence` で、問い合わせのためだけのセッションを CLI 側に残さない。
#[instrument(ret)]
pub(crate) async fn fetch(cwd: &Path) -> Result<Vec<CliModel>, String> {
    let cli = crate::writing_agent::resolve_cli_path().unwrap_or_else(|| PathBuf::from("claude"));
    if let Some(reason) = crate::writing_agent::unsupported_cli_reason(&cli) {
        return Err(reason.to_string());
    }
    let started = std::time::Instant::now();
    let result = match tokio::time::timeout(FETCH_TIMEOUT, query(&cli, cwd)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "{}秒以内に応答がありませんでした",
            FETCH_TIMEOUT.as_secs()
        )),
    };
    debug!(
        "claude_models: 問い合わせに {} ms かかりました",
        started.elapsed().as_millis()
    );
    result
}

/// `initialize` を送り、対応する `control_response` が来るまで stdout を読む。
///
/// stdin は応答を読み終えるまで閉じない(閉じると CLI が応答前に終了しうる)。
/// 読み終えたらプロセスは用済みなので明示的に kill する(タイムアウトで future ごと
/// 捨てられた場合も `kill_on_drop` で後始末される)。
#[instrument]
async fn query(cli: &Path, cwd: &Path) -> Result<Vec<CliModel>, String> {
    let mut child = tokio::process::Command::new(cli)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--setting-sources",
            "project",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // 読まない stderr を piped にするとバッファが埋まって CLI が止まりうるので捨てる。
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            format!(
                "claude CLI を起動できませんでした({}): {}",
                cli.display(),
                e
            )
        })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or("claude CLI の stdin が取れません")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("claude CLI の stdout が取れません")?;

    let request = serde_json::json!({
        "type": "control_request",
        "request_id": REQUEST_ID,
        "request": { "subtype": "initialize" },
    });
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(|e| format!("initialize を送れませんでした: {}", e))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("initialize を送れませんでした: {}", e))?;

    let mut lines = BufReader::new(stdout).lines();
    let result = loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(result) = parse_initialize_response(&line) {
                    break result;
                }
            }
            Ok(None) => break Err("claude CLI が応答前に終了しました".to_string()),
            Err(e) => break Err(format!("claude CLI の出力を読めませんでした: {}", e)),
        }
    };

    drop(stdin);
    if let Err(e) = child.kill().await {
        warn!(
            "claude_models: 問い合わせ用の claude を終了できませんでした: {}",
            e
        );
    }
    result
}

/// stdout の1行を読み、[`REQUEST_ID`] に対する `control_response` ならその結果を返す。
/// それ以外の行(hook の通知など)は `None` で読み飛ばす。
///
/// `models` の各要素は1件ずつ読み、形の合わない要素だけを捨てる
/// (CLI 側の形式変更で1件崩れても、一覧全体を失わないため)。
#[instrument(ret, level = "trace")]
fn parse_initialize_response(line: &str) -> Option<Result<Vec<CliModel>, String>> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "control_response" {
        return None;
    }
    let response = value.get("response")?;
    if response.get("request_id")?.as_str()? != REQUEST_ID {
        return None;
    }
    if response.get("subtype").and_then(serde_json::Value::as_str) != Some("success") {
        let error = response
            .get("error")
            .map(ToString::to_string)
            .unwrap_or_else(|| response.to_string());
        return Some(Err(format!("initialize が失敗しました: {}", error)));
    }

    let models: Vec<CliModel> = response
        .get("response")
        .and_then(|r| r.get("models"))
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<CliModel>(item.clone()).ok())
                .filter(|m| !m.value.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if models.is_empty() {
        return Some(Err(
            "initialize の応答に models がありませんでした".to_string()
        ));
    }
    Some(Ok(models))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// claude 2.1.300 が実際に返した応答(抜粋)。
    const SAMPLE: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"fifty_four_models","response":{"commands":[],"models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Use the default model (currently Opus 5.5) · $4/$20 per Mtok","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"]},{"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus","description":"Opus 5.5 · Best for everyday, complex tasks · $4/$20 per Mtok"},{"value":"claude-fable-5-1[1m]","resolvedModel":"claude-fable-5-1","displayName":"Fable","description":"Fable 5.1 · Most capable for your hardest and longest-running tasks · $10/$50 per Mtok"},{"value":"sonnet","displayName":"Sonnet"},{"value":"haiku"}],"pid":1234}}}"#;

    #[test]
    fn test_parse_initialize_response_reads_models() {
        let models = parse_initialize_response(SAMPLE).unwrap().unwrap();
        let values: Vec<&str> = models.iter().map(|m| m.value.as_str()).collect();
        assert_eq!(
            values,
            ["default", "opus", "claude-fable-5-1[1m]", "sonnet", "haiku"]
        );
        assert_eq!(
            models[0].display_name.as_deref(),
            Some("Default (recommended)")
        );
        assert!(
            models[2]
                .description
                .as_deref()
                .unwrap()
                .starts_with("Fable 5.1")
        );
        assert_eq!(models[0].supports_effort, Some(true));
        assert_eq!(
            models[0].supported_effort_levels,
            ["low", "medium", "high", "xhigh", "max"]
        );
        // displayName / description / effort 関連が無い要素も読める。
        assert_eq!(models[4].display_name, None);
        assert_eq!(models[4].supports_effort, None);
        assert!(models[4].supported_effort_levels.is_empty());
    }

    #[test]
    fn test_parse_initialize_response_skips_other_lines() {
        let hook = r#"{"type":"system","subtype":"hook_started","session_id":"s"}"#;
        assert!(parse_initialize_response(hook).is_none());
        let other_id = SAMPLE.replace("fifty_four_models", "someone_else");
        assert!(parse_initialize_response(&other_id).is_none());
        assert!(parse_initialize_response("not json").is_none());
    }

    #[test]
    fn test_parse_initialize_response_drops_malformed_entries() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"fifty_four_models","response":{"models":[{"value":"opus"},{"displayName":"no value"},{"value":""},42]}}}"#;
        let models = parse_initialize_response(line).unwrap().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].value, "opus");
    }

    #[test]
    fn test_parse_initialize_response_reports_error_and_empty() {
        let error = r#"{"type":"control_response","response":{"subtype":"error","request_id":"fifty_four_models","error":"boom"}}"#;
        assert!(
            parse_initialize_response(error)
                .unwrap()
                .unwrap_err()
                .contains("boom")
        );
        let empty = r#"{"type":"control_response","response":{"subtype":"success","request_id":"fifty_four_models","response":{"models":[]}}}"#;
        assert!(parse_initialize_response(empty).unwrap().is_err());
    }

    /// 実際の `claude` CLI に問い合わせる。CLI のインストールとログインが要るので通常は走らせない。
    /// `cargo test -- --ignored test_fetch_real_cli` で手動確認する。
    #[tokio::test]
    #[ignore]
    async fn test_fetch_real_cli() {
        let models = fetch(&std::env::temp_dir()).await.unwrap();
        assert!(models.iter().any(|m| m.value == "default"), "{models:?}");
    }
}
