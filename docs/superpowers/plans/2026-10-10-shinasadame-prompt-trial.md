# shinasadame プロンプト試行機能 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** shinasadame のレコード詳細画面から、記録済みプロンプトを編集して任意の LLM に再実行し、元の出力と見比べられるようにする。

**Architecture:** `fifty_four_lsp` に feature `replay` を足し、`--replay` で axum の HTTP サーバー(`POST /v1/replay`、`GET /healthz`)として起動する。LLM 呼び出しは既存の `llm.rs`(`LlmClientBuilder`)を通す。これを別 Pod(Deployment + ClusterIP Service)に置き、shinasadame(FastAPI)が `httpx` で呼ぶ。試行結果は保存せず htmx の部分テンプレートで表示する。

**Tech Stack:** Rust(axum 0.8、tokio、既存の genai ベース `llm.rs`)、Python(FastAPI、Jinja2、htmx、httpx)、Docker、k8s(kustomize)。

**Spec:** `docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md`

## Global Constraints

- `replay` は Cargo feature。**既定では無効**(`default = ["otel"]` のまま)。`axum` は `replay` の optional 依存にする。
- `--replay` は feature 無効のビルドで渡されたら、stderr にメッセージを出して終了コード **2** で終了する(LSP として黙って起動しない)。
- `POST /v1/replay` の本文は 1 MiB まで。応答は 1,000,000 **文字**(バイトではない)で切り詰め、`truncated: true` を返す。
- `timeout_secs` の既定 120、上限 600。同時実行数の既定 4(`REPLAY_MAX_CONCURRENCY`)。待機は拒否ではなくキュー。
- エラー JSON は `{"error": {"kind": "...", "message": "..."}}`。`kind` と HTTP: `bad_request`=400 / `auth`=502 / `timeout`=504 / `llm`=502 / `internal`=500。
- 標準出力に何も書かない(ログは stderr / OTLP)。
- API キーは replay の Pod だけが持つ。shinasadame の Pod の環境変数・画面・ログ・応答に出さない。
- tool(`character_info` / `plot_info`)は渡さない。画面に「tool なしで実行します。元の実行とは条件が異なります」と注記する。
- 試行結果は保存しない(DB に書かない)。
- `httpx` は shinasadame の**実行時**依存にする(現状は dev 依存)。
- 既存テストの方針に合わせる: Rust のテストは各ファイル末尾の `#[cfg(test)] mod tests`、一時ディレクトリは `tempfile`、環境変数は `crate::test_util::EnvGuard`。
- `cargo fmt` は変更したファイルにだけ効かせる(無関係なファイルを整形しない)。
- コミットメッセージの末尾に `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>` を付ける。プッシュはしない。

## Review Focus

仕様が暗黙に求めているが、各タスクの正常系テストでは触れにくい入力・状態(起きやすい順)。それぞれ担当タスクのテストで固定する。

1. **空または空白だけのプロンプト・空のモデル名**(`"google/"` のようにモデル部が空のものを含む)→ 400 `bad_request`。panic しない(Task 2)。
2. **未知の provider、`cloudflare/...`**(`account_id` を渡せず動かない)→ 400 `bad_request`。`LlmClientBuilder::from_name` は不正名で `unwrap` panic するため、必ず事前検証する(Task 2)。
3. **LLM の出力に HTML(`<script>`)が含まれる**、編集欄のプロンプトに `</textarea>` が含まれる → 画面ではエスケープされる(Task 7)。
4. **replay サービスが停止中・接続不能** → shinasadame の画面は 500 にならず、エラー表示の部分テンプレートが返る(Task 6、7)。
5. **マルチバイト(日本語・絵文字)の長い応答の切り詰め** → 文字境界で切り、不正な UTF-8 を出さない(Task 2)。

---

## File Structure

| ファイル | 役割 |
|---|---|
| `lsp/Cargo.toml`(変更) | feature `replay`、optional な `axum`、dev 依存 `tower` / `http-body-util` |
| `lsp/src/replay.rs`(新規) | リクエスト・応答型、検証、エラー分類、`ClientFactory`、ハンドラ、`router`、`serve`、`main` |
| `lsp/src/main.rs`(変更) | `mod replay;`(feature 付き)、`--replay` の分岐 |
| `lsp/tests/replay_flag.rs`(新規) | feature 無効時に `--replay` が終了コード 2 になることの結合テスト |
| `.github/workflows/test.yml`(変更) | lsp ジョブに `--features replay` のテストを追加 |
| `lsp/Dockerfile.replay`(新規) | replay 用イメージ(多段ビルド) |
| `shinasadame/k8s/base/replay-deployment.yaml`、`replay-service.yaml`(新規)、`kustomization.yaml`・`deployment.yaml`(変更) | 別 Pod の配備 |
| `shinasadame/pyproject.toml`、`uv.lock`(変更) | `httpx` を実行時依存へ |
| `shinasadame/src/shinasadame/config.py`(変更) | `replay_url()`、`replay_default_timeout_secs()`、`model_choices()` |
| `shinasadame/src/shinasadame/replay.py`(新規) | `ReplayClient`、結果型、`ReplayUnavailable` |
| `shinasadame/src/shinasadame/records.py`(変更) | `PROMPT_TABLES`、`original_output()` |
| `shinasadame/src/shinasadame/app.py`(変更) | `get_replay_client`、`POST .../try`、詳細画面へのコンテキスト追加 |
| `shinasadame/src/shinasadame/templates/_trial_result.html`(新規)、`record_detail.html`(変更) | 「試す」パネルと結果表示 |
| `shinasadame/tests/test_replay_client.py`、`test_trial.py`(新規) | Python のテスト |
| `docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md`(変更) | システムプロンプトの未確認事項の解消 |

## 前提として確認済みの事実

- LSP の補完・コードアクションは `build_client_async(cfg, "system.md")` で**システムプロンプト `system.md` を付けて**呼んでいる(`lsp/src/backend.rs:854`、`:884`)。記録される `prompt` はそのユーザー部分なので、再実行も既定で `system.md`(`crate::assets::load("system.md")`)を付ける。リクエストの `system` で上書き(空文字ならなし)できるようにする。
- `Provider::from_name(&str) -> Result<Provider, String>` は不正名で `Err`。一方 `LlmClientBuilder::from_name(&str) -> Self` は内部で `unwrap` するので、**`Provider::from_name` で先に検証する**。
- `use_llm_with_option` は、オプションが空なら `reset_options()` と `proc` の呼び出しだけをする。replay では新規クライアントを毎回作るため、**使わずに `client.add(...)` と `client.chat()` を直接呼ぶ**(高階ライフタイムの `AsyncFnOnce` を axum ハンドラの `Send` 制約に通す手間も避けられる)。
- 認証エラーを表す専用の `LlmError` は無く、`GenericError { message }` に載る。そのため `auth` の判定はメッセージの文字列に基づく(Task 2 で固定)。
- API キーの環境変数名: `GEMINI_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `XAI_API_KEY`(`lsp/src/llm.rs:886-901`)。

---

### Task 1: feature `replay` と `--replay` の分岐

**Files:**
- Modify: `lsp/Cargo.toml`
- Modify: `lsp/src/main.rs`(`mod` 宣言、`main()` の先頭付近)
- Create: `lsp/src/replay.rs`(この段階では `main()` の骨組みだけ)
- Create: `lsp/tests/replay_flag.rs`

**Interfaces:**
- Produces: `crate::replay::main() -> ()`(feature 有効時のみ存在。サーバー起動は Task 4 で実装)。`cfg(not(feature = "replay"))` では `--replay` が終了コード 2。

- [ ] **Step 1: 失敗するテストを書く(feature 無効時の終了コード)**

`lsp/tests/replay_flag.rs`:

```rust
//! `--replay` の分岐の結合テスト。バイナリを実際に起動して終了コードを見る。

#[cfg(not(feature = "replay"))]
#[test]
fn replay_flag_without_feature_exits_with_code_2() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fifty_four_lsp"))
        .arg("--replay")
        .output()
        .expect("バイナリを起動できる");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("replay feature"),
        "stderr にビルド方法の案内が出る: {stderr}"
    );
    // 標準出力は LSP プロトコル用なので、何も書かない
    assert!(output.stdout.is_empty());
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cd lsp && cargo test --test replay_flag`
Expected: FAIL(`--replay` は無視されて LSP として起動し、標準入力が閉じて終了コード 0 になる。または待ち続ける。待ち続けた場合は Ctrl+C で止め、Step 3 に進む)

- [ ] **Step 3: `Cargo.toml` に feature と依存を足す**

`lsp/Cargo.toml` の `[features]` に追加:

```toml
replay = ["dep:axum"]
```

`[dependencies]` の末尾(`tracing-log` の行の後)に追加:

```toml
axum = { version = "0.8", optional = true }
```

`[dev-dependencies]` に追加(`tokio` の `full` は `test-util` を含まない。時間を止めるタイムアウトのテストに要る):

```toml
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
tokio = { version = "1", features = ["test-util"] }
```

- [ ] **Step 4: `main.rs` に分岐を足す**

`lsp/src/main.rs` の `mod quality;` と `mod references;` の間に追加:

```rust
#[cfg(feature = "replay")]
mod replay;
```

`fn main()` の先頭(`let acp = ...` の直前)に追加:

```rust
    if std::env::args().skip(1).any(|a| a == "--replay") {
        #[cfg(feature = "replay")]
        {
            return replay::main();
        }
        // LSP の stdio は JSON-RPC 用なので、黙って LSP として起動してはいけない。
        #[cfg(not(feature = "replay"))]
        {
            eprintln!(
                "fifty_four_lsp: --replay は replay feature なしでビルドされています \
                 (cargo build --release -p fifty_four_lsp --features replay)"
            );
            std::process::exit(2);
        }
    }
```

- [ ] **Step 5: `replay.rs` の骨組みを作る**

`lsp/src/replay.rs`:

```rust
//! `--replay`: 記録されたプロンプトを任意の LLM に再実行する HTTP サーバー(feature `replay`)。
//!
//! 仕様: docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md

/// `--replay` のエントリポイント。サーバーの起動は後続のタスクで実装する。
pub(crate) fn main() {
    eprintln!("fifty_four_lsp --replay: not implemented yet");
    std::process::exit(1);
}
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cd lsp && cargo test --test replay_flag`
Expected: PASS(1 件)

Run: `cd lsp && cargo build --features replay`
Expected: 成功(警告なし。`main` は使われているので dead code にならない)

- [ ] **Step 7: コミット**

```bash
git add lsp/Cargo.toml lsp/src/main.rs lsp/src/replay.rs lsp/tests/replay_flag.rs
git commit -m "feat(lsp): replay feature と --replay フラグの分岐を追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 2: 型・検証・エラー分類・切り詰め(純粋関数)

**Files:**
- Modify: `lsp/src/replay.rs`

**Interfaces:**
- Produces:
  - `pub(crate) struct ReplayRequest { pub model: String, pub prompt: String, pub timeout_secs: Option<u64>, pub system: Option<String> }`(`Deserialize`)
  - `pub(crate) struct ReplayResponse { pub response: String, pub model: String, pub elapsed_ms: u64, pub truncated: bool }`(`Serialize`)
  - `pub(crate) enum ErrorKind { BadRequest, Auth, Timeout, Llm, Internal }`、`ErrorKind::status(self) -> StatusCode`
  - `pub(crate) struct ReplayError { pub kind: ErrorKind, pub message: String }`(`IntoResponse`)
  - `ReplayRequest::validate(&self) -> Result<Duration, ReplayError>`(検証して有効なタイムアウトを返す)
  - `pub(crate) fn classify_llm_error(e: &LlmError) -> ReplayError`
  - `pub(crate) fn truncate_chars(text: String, max: usize) -> (String, bool)`
  - 定数: `DEFAULT_TIMEOUT_SECS`(120)、`MAX_TIMEOUT_SECS`(600)、`MAX_BODY_BYTES`(1 MiB)、`MAX_RESPONSE_CHARS`(1,000,000)、`DEFAULT_MAX_CONCURRENCY`(4)、`DEFAULT_LISTEN`(`"0.0.0.0:8080"`)

- [ ] **Step 1: 失敗するテストを書く**

`lsp/src/replay.rs` の末尾に追加:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str, prompt: &str) -> ReplayRequest {
        ReplayRequest {
            model: model.to_string(),
            prompt: prompt.to_string(),
            timeout_secs: None,
            system: None,
        }
    }

    #[test]
    fn test_validate_accepts_known_provider_and_returns_default_timeout() {
        let t = req("google/gemini-3.1-pro", "こんにちは").validate().unwrap();
        assert_eq!(t, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn test_validate_clamps_timeout_to_max() {
        let mut r = req("google/gemini-3.1-pro", "x");
        r.timeout_secs = Some(10_000);
        assert_eq!(r.validate().unwrap(), Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn test_validate_rejects_zero_timeout() {
        let mut r = req("google/gemini-3.1-pro", "x");
        r.timeout_secs = Some(0);
        assert_eq!(r.validate().unwrap_err().kind, ErrorKind::BadRequest);
    }

    #[test]
    fn test_validate_rejects_empty_and_whitespace_prompt() {
        for p in ["", "   ", "\n\t "] {
            let e = req("google/gemini-3.1-pro", p).validate().unwrap_err();
            assert_eq!(e.kind, ErrorKind::BadRequest, "prompt={p:?}");
        }
    }

    #[test]
    fn test_validate_rejects_empty_model_and_empty_model_part() {
        for m in ["", "  ", "google/", "lmstudio@localhost:1234/"] {
            let e = req(m, "x").validate().unwrap_err();
            assert_eq!(e.kind, ErrorKind::BadRequest, "model={m:?}");
        }
    }

    #[test]
    fn test_validate_rejects_unknown_provider_without_panicking() {
        let e = req("nosuch/model", "x").validate().unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("nosuch/model"), "{}", e.message);
    }

    #[test]
    fn test_validate_rejects_cloudflare_because_account_id_cannot_be_given() {
        let e = req("cloudflare/@cf/meta/llama-3.1-8b-instruct", "x")
            .validate()
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("cloudflare"), "{}", e.message);
    }

    #[test]
    fn test_classify_auth_errors_by_message() {
        for msg in [
            "HTTP 401 Unauthorized",
            "status 403 Forbidden",
            "API key not valid",
            "env var GEMINI_API_KEY not found (api_key)",
            "authentication failed",
        ] {
            let e = classify_llm_error(&LlmError::GenericError {
                message: msg.to_string(),
            });
            assert_eq!(e.kind, ErrorKind::Auth, "msg={msg}");
            assert!(e.message.contains(msg), "原文を失わない: {}", e.message);
        }
    }

    #[test]
    fn test_classify_other_generic_error_is_llm() {
        let e = classify_llm_error(&LlmError::GenericError {
            message: "model overloaded".to_string(),
        });
        assert_eq!(e.kind, ErrorKind::Llm);
    }

    #[test]
    fn test_classify_busy_is_llm_and_not_initialized_is_internal() {
        assert_eq!(
            classify_llm_error(&LlmError::LlmBusy { retry_after: 3 }).kind,
            ErrorKind::Llm
        );
        assert_eq!(
            classify_llm_error(&LlmError::NotInitialized).kind,
            ErrorKind::Internal
        );
    }

    #[test]
    fn test_error_kind_maps_to_http_status() {
        assert_eq!(ErrorKind::BadRequest.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorKind::Auth.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(ErrorKind::Timeout.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(ErrorKind::Llm.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(ErrorKind::Internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_truncate_chars_keeps_short_text_untouched() {
        assert_eq!(truncate_chars("短い".to_string(), 10), ("短い".to_string(), false));
    }

    #[test]
    fn test_truncate_chars_cuts_on_char_boundary_for_multibyte_text() {
        // 絵文字(4 バイト)と日本語(3 バイト)が混ざっても、文字数で切って UTF-8 を壊さない。
        let text = "あい😀う😀え".to_string();
        let (cut, truncated) = truncate_chars(text, 4);
        assert_eq!(cut, "あい😀う");
        assert!(truncated);
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_chars_exact_limit_is_not_truncated() {
        assert_eq!(truncate_chars("あいう".to_string(), 3), ("あいう".to_string(), false));
    }
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: FAIL(コンパイルエラー: `ReplayRequest` などが未定義)

- [ ] **Step 3: 実装する**

`lsp/src/replay.rs` の `main()` より前(ファイル冒頭のドキュメントコメントの直後)に追加:

```rust
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::llm::{LlmError, Provider};

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(crate) const MAX_TIMEOUT_SECS: u64 = 600;
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_CHARS: usize = 1_000_000;
pub(crate) const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

#[derive(Debug, Deserialize)]
pub(crate) struct ReplayRequest {
    /// `provider/model` または `lmstudio@host:port/model`(`Provider::from_name` の書式)
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// システムプロンプト。省略時は LSP の執筆支援と同じ `system.md`。空文字ならシステムプロンプトなし。
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct ReplayResponse {
    pub response: String,
    pub model: String,
    pub elapsed_ms: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    BadRequest,
    Auth,
    Timeout,
    Llm,
    Internal,
}

impl ErrorKind {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Auth | ErrorKind::Llm => StatusCode::BAD_GATEWAY,
            ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ReplayError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ReplayError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }
}

impl IntoResponse for ReplayError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "kind": self.kind, "message": self.message }
        });
        (self.kind.status(), Json(body)).into_response()
    }
}

impl ReplayRequest {
    /// 入力を検証し、有効なタイムアウトを返す。`LlmClientBuilder::from_name` は不正名で
    /// `unwrap` により panic するため、クライアントを作る前に必ずここを通す。
    pub(crate) fn validate(&self) -> Result<Duration, ReplayError> {
        if self.prompt.trim().is_empty() {
            return Err(ReplayError::bad_request("prompt が空です"));
        }
        let model = self.model.trim();
        if model.is_empty() {
            return Err(ReplayError::bad_request("model が空です"));
        }
        // `google/` や `lmstudio@host:1234/` のようにモデル部が空のものを弾く
        if model.rsplit('/').next().is_none_or(|m| m.trim().is_empty()) {
            return Err(ReplayError::bad_request(format!(
                "model のモデル名の部分が空です: {model}"
            )));
        }
        // account_id を渡す経路が無く、Cloudflare は組み立てる URL が不正になる
        if model.starts_with("cloudflare/") {
            return Err(ReplayError::bad_request(
                "cloudflare は replay では未対応です(account_id を指定できません)",
            ));
        }
        Provider::from_name(model).map_err(ReplayError::bad_request)?;

        let secs = match self.timeout_secs {
            None => DEFAULT_TIMEOUT_SECS,
            Some(0) => return Err(ReplayError::bad_request("timeout_secs は 1 以上にしてください")),
            Some(n) => n.min(MAX_TIMEOUT_SECS),
        };
        Ok(Duration::from_secs(secs))
    }
}

/// `LlmError` を `ReplayError` に写す。認証エラー専用のバリアントが無く `GenericError` に
/// 載ってくるため、`auth` はメッセージの文字列で判定する(大文字小文字を無視)。
pub(crate) fn classify_llm_error(e: &LlmError) -> ReplayError {
    match e {
        LlmError::GenericError { message } => {
            let lower = message.to_lowercase();
            const AUTH_HINTS: [&str; 7] = [
                "401",
                "403",
                "unauthorized",
                "forbidden",
                "api key",
                "api_key",
                "authentication",
            ];
            if AUTH_HINTS.iter().any(|h| lower.contains(h)) {
                ReplayError::new(ErrorKind::Auth, format!("認証に失敗しました: {message}"))
            } else {
                ReplayError::new(ErrorKind::Llm, message.clone())
            }
        }
        LlmError::NotInitialized => ReplayError::new(ErrorKind::Internal, e.to_string()),
        other => ReplayError::new(ErrorKind::Llm, other.to_string()),
    }
}

/// 文字数(バイトではない)で切り詰める。切り詰めたかどうかも返す。
pub(crate) fn truncate_chars(text: String, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((byte_idx, _)) => (text[..byte_idx].to_string(), true),
        None => (text, false),
    }
}
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: PASS(14 件)

もし `AUTH_HINTS` の件数がコンパイルエラー(配列長の不一致)になったら、`[&str; 7]` の `7` を実際の要素数に合わせる。

- [ ] **Step 5: コミット**

```bash
cd lsp && cargo fmt -- src/replay.rs 2>/dev/null; cd ..
git add lsp/src/replay.rs
git commit -m "feat(lsp): replay の型・検証・エラー分類・切り詰めを追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

(`cargo fmt -- <file>` が他のファイルを整形していないか、`git status` で確認する。無関係な差分が出たら `git checkout -- <そのファイル>` で戻す)

---

### Task 3: ハンドラ・ルーター・ClientFactory

**Files:**
- Modify: `lsp/src/replay.rs`
- Modify: `docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md`(システムプロンプトの未確認事項を解消)

**Interfaces:**
- Consumes: Task 2 の型と `validate` / `classify_llm_error` / `truncate_chars`。
- Produces:
  - `#[async_trait] pub(crate) trait ClientFactory: Send + Sync { async fn create(&self, model: &str, system: Option<String>) -> Result<Box<dyn LlmInterface>, ReplayError>; }`
  - `pub(crate) struct RealFactory;`(`ClientFactory` の本番実装)
  - `pub(crate) struct AppState { pub factory: Arc<dyn ClientFactory>, pub permits: Arc<Semaphore> }` と `AppState::new(factory: Arc<dyn ClientFactory>, max_concurrency: usize) -> Arc<AppState>`
  - `pub(crate) fn router(state: Arc<AppState>) -> Router`

- [ ] **Step 1: 失敗するテストを書く**

`lsp/src/replay.rs` の `mod tests` の先頭の `use super::*;` の直後に追加(既存のテストはそのまま):

```rust
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use genai::chat::{ChatResponseFormat, ReasoningEffort, ServiceTier, Verbosity};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::llm::{Content, LlmInterface, LlmTool, ModelCapability};

    /// テスト専用の最小 LlmInterface。`Behavior` どおりに `chat()` する。
    #[derive(Debug, Clone)]
    enum Behavior {
        Reply(String),
        Fail(String),
        Hang,
    }

    #[derive(Debug)]
    struct StubLlm {
        behavior: Behavior,
        added: Vec<String>,
        seen: std::sync::Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl LlmInterface for StubLlm {
        async fn chat(&mut self) -> Result<String, LlmError> {
            *self.seen.lock().unwrap() = self.added.clone();
            match &self.behavior {
                Behavior::Reply(s) => Ok(s.clone()),
                Behavior::Fail(m) => Err(LlmError::GenericError { message: m.clone() }),
                Behavior::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
        async fn with_model(&mut self, _model: &str) -> Result<String, LlmError> {
            Ok(String::new())
        }
        fn add(&mut self, prompt: Content) {
            self.added.push(prompt.to_string());
        }
        fn build_content(&self) -> String {
            String::new()
        }
        fn get_model(&self) -> &str {
            "stub-model"
        }
        fn clear(&mut self) {}
        fn cache(&mut self, _prompt: Content) -> Result<String, LlmError> {
            Ok(String::new())
        }
        fn fetch(&self, _hash: &str) -> Option<&Content> {
            None
        }
        fn fetch_all(&self) -> Vec<Content> {
            Vec::new()
        }
        fn remove(&mut self, _hash: String) {}
        fn capabilities(&self) -> ModelCapability {
            ModelCapability::empty()
        }
        fn max_tokens(&mut self, _n: u32) {}
        fn temperature(&mut self, _v: f64) {}
        fn top_p(&mut self, _v: f64) {}
        fn stop_sequences(&mut self, _seqs: Vec<String>) {}
        fn seed(&mut self, _v: u64) {}
        fn reasoning_effort(&mut self, _effort: ReasoningEffort) {}
        fn reasoning_level(&mut self, _level: f64) {}
        fn response_format(&mut self, _fmt: ChatResponseFormat) {}
        fn service_tier(&mut self, _tier: ServiceTier) {}
        fn verbosity(&mut self, _v: Verbosity) {}
        fn add_tool(&mut self, _tool: Box<dyn LlmTool>) {}
        async fn respond_tool(
            &self,
            _tools: &[genai::chat::ToolCall],
        ) -> Result<genai::chat::ChatRequest, LlmError> {
            Err(LlmError::NotImplemented)
        }
    }

    /// `ClientFactory` のテスト用実装。作成回数と、`chat()` 時にクライアントへ渡されていたプロンプトを記録する。
    struct StubFactory {
        behavior: Behavior,
        created: AtomicUsize,
        last_system: StdMutex<Option<Option<String>>>,
        seen_prompts: std::sync::Arc<StdMutex<Vec<String>>>,
    }

    impl StubFactory {
        fn new(behavior: Behavior) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                behavior,
                created: AtomicUsize::new(0),
                last_system: StdMutex::new(None),
                seen_prompts: std::sync::Arc::new(StdMutex::new(Vec::new())),
            })
        }
    }

    #[async_trait]
    impl ClientFactory for StubFactory {
        async fn create(
            &self,
            _model: &str,
            system: Option<String>,
        ) -> Result<Box<dyn LlmInterface>, ReplayError> {
            self.created.fetch_add(1, Ordering::SeqCst);
            *self.last_system.lock().unwrap() = Some(system);
            Ok(Box::new(StubLlm {
                behavior: self.behavior.clone(),
                added: Vec::new(),
                seen: self.seen_prompts.clone(),
            }))
        }
    }

    fn app(factory: std::sync::Arc<StubFactory>, max_concurrency: usize) -> axum::Router {
        router(AppState::new(factory, max_concurrency))
    }

    async fn post_json(app: axum::Router, body: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/replay")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.oneshot(request).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    const OK_BODY: &str = r#"{"model":"google/gemini-3.1-pro","prompt":"続きを書いて"}"#;

    #[tokio::test]
    async fn test_replay_returns_response_model_and_elapsed() {
        let f = StubFactory::new(Behavior::Reply("返答です".to_string()));
        let (status, json) = post_json(app(f.clone(), 4), OK_BODY).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["response"], "返答です");
        assert_eq!(json["model"], "google/gemini-3.1-pro");
        assert_eq!(json["truncated"], false);
        assert!(json["elapsed_ms"].is_u64());
        // 記録されたプロンプトがそのままユーザープロンプトとして渡る
        assert_eq!(*f.seen_prompts.lock().unwrap(), vec!["続きを書いて".to_string()]);
    }

    #[tokio::test]
    async fn test_replay_passes_system_override_and_default_to_factory() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        post_json(app(f.clone(), 4), OK_BODY).await;
        assert_eq!(*f.last_system.lock().unwrap(), Some(None), "省略は None(=system.md)");

        let body = r#"{"model":"google/gemini-3.1-pro","prompt":"x","system":""}"#;
        post_json(app(f.clone(), 4), body).await;
        assert_eq!(*f.last_system.lock().unwrap(), Some(Some(String::new())));
    }

    #[tokio::test]
    async fn test_replay_bad_json_is_bad_request_not_422() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let (status, json) = post_json(app(f.clone(), 4), "これはJSONではない").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
        assert_eq!(f.created.load(Ordering::SeqCst), 0, "検証で弾けばクライアントを作らない");
    }

    #[tokio::test]
    async fn test_replay_missing_field_is_bad_request() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let (status, json) = post_json(app(f, 4), r#"{"model":"google/gemini-3.1-pro"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
    }

    #[tokio::test]
    async fn test_replay_validation_errors_do_not_create_a_client() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        for body in [
            r#"{"model":"google/gemini-3.1-pro","prompt":"   "}"#,
            r#"{"model":"google/","prompt":"x"}"#,
            r#"{"model":"nosuch/m","prompt":"x"}"#,
        ] {
            let (status, json) = post_json(app(f.clone(), 4), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
            assert_eq!(json["error"]["kind"], "bad_request");
        }
        assert_eq!(f.created.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_replay_auth_error_is_502_auth() {
        let f = StubFactory::new(Behavior::Fail("HTTP 401 Unauthorized".to_string()));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["error"]["kind"], "auth");
    }

    #[tokio::test]
    async fn test_replay_llm_error_is_502_llm() {
        let f = StubFactory::new(Behavior::Fail("model overloaded".to_string()));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["error"]["kind"], "llm");
        assert!(json["error"]["message"].as_str().unwrap().contains("model overloaded"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_replay_timeout_is_504_and_server_keeps_working() {
        let hang = StubFactory::new(Behavior::Hang);
        let body = r#"{"model":"google/gemini-3.1-pro","prompt":"x","timeout_secs":1}"#;
        let (status, json) = post_json(app(hang, 4), body).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json["error"]["kind"], "timeout");
    }

    #[tokio::test]
    async fn test_replay_truncates_long_multibyte_response() {
        let long = "あ😀".repeat(MAX_RESPONSE_CHARS); // 2 * MAX 文字
        let f = StubFactory::new(Behavior::Reply(long));
        let (status, json) = post_json(app(f, 4), OK_BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["truncated"], true);
        assert_eq!(json["response"].as_str().unwrap().chars().count(), MAX_RESPONSE_CHARS);
    }

    #[tokio::test]
    async fn test_replay_rejects_body_over_limit_as_bad_request() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let huge = "あ".repeat(MAX_BODY_BYTES); // バイト数では上限の 3 倍
        let body = serde_json::json!({"model": "google/gemini-3.1-pro", "prompt": huge}).to_string();
        let (status, json) = post_json(app(f.clone(), 4), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["kind"], "bad_request");
        assert_eq!(f.created.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_replay_waits_for_a_permit_instead_of_rejecting() {
        // 同時実行 1。2 件同時に投げても、どちらも 200 で終わる(後続は待機)。
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let a = app(f.clone(), 1);
        let (r1, r2) = tokio::join!(post_json(a.clone(), OK_BODY), post_json(a, OK_BODY));
        assert_eq!(r1.0, StatusCode::OK);
        assert_eq!(r2.0, StatusCode::OK);
        assert_eq!(f.created.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_healthz_returns_ok() {
        let f = StubFactory::new(Behavior::Reply("ok".to_string()));
        let request = Request::builder().uri("/healthz").body(Body::empty()).unwrap();
        let res = app(f, 4).oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"ok");
    }
```

- [ ] **Step 2: 失敗を確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: FAIL(コンパイルエラー: `ClientFactory`、`router`、`AppState` が未定義)

- [ ] **Step 3: 実装する**

`lsp/src/replay.rs` の既存の `use` 群に追加:

```rust
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, post};
use tokio::sync::Semaphore;

use crate::llm::{Content, LlmClientBuilder, LlmInterface};
```

(`use crate::llm::{LlmError, Provider};` は `use crate::llm::{Content, LlmClientBuilder, LlmError, LlmInterface, Provider};` に統合する)

`truncate_chars` の後(`main()` の前)に追加:

```rust
/// LLM クライアントの生成を差し替えられるようにするための境界(テストはネットワークに出ない)。
#[async_trait]
pub(crate) trait ClientFactory: Send + Sync {
    /// `system`: `None` は既定(`system.md`)、`Some("")` はシステムプロンプトなし、それ以外はその内容。
    async fn create(
        &self,
        model: &str,
        system: Option<String>,
    ) -> Result<Box<dyn LlmInterface>, ReplayError>;
}

/// 本番用。LSP の執筆支援と同じ経路(`LlmClientBuilder`)でクライアントを作る。
pub(crate) struct RealFactory;

#[async_trait]
impl ClientFactory for RealFactory {
    async fn create(
        &self,
        model: &str,
        system: Option<String>,
    ) -> Result<Box<dyn LlmInterface>, ReplayError> {
        let sys_prompt = match system {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
            None => crate::assets::load("system.md"),
        };
        // `validate()` で `Provider::from_name` が通っているので、ここで panic しない。
        Ok(LlmClientBuilder::from_name(model)
            .sys_prompt(sys_prompt)
            .build_async()
            .await)
    }
}

pub(crate) struct AppState {
    pub factory: Arc<dyn ClientFactory>,
    pub permits: Arc<Semaphore>,
}

impl AppState {
    pub(crate) fn new(factory: Arc<dyn ClientFactory>, max_concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            factory,
            permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
        })
    }
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/replay", post(replay_handler))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn replay_handler(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ReplayRequest>, JsonRejection>,
) -> Result<Json<ReplayResponse>, ReplayError> {
    // 不正な JSON・Content-Type 欠落・本文が上限超過は、すべて 400 `bad_request` に揃える。
    let Json(req) = body
        .map_err(|e| ReplayError::bad_request(format!("リクエストを解釈できません: {}", e.body_text())))?;
    let timeout = req.validate()?;

    // 同時実行の上限。超えた分は拒否せず待機する(待ちが長ければクライアント側のタイムアウトが効く)。
    let _permit = state
        .permits
        .acquire()
        .await
        .map_err(|_| ReplayError::new(ErrorKind::Internal, "同時実行の制御に失敗しました"))?;

    let started = Instant::now();
    let mut client = state.factory.create(req.model.trim(), req.system.clone()).await?;
    client.add(Content::Text(req.prompt.clone()));

    // オプションが空なので `use_llm_with_option` は使わず、新規クライアントを直接呼ぶ。
    let text = match tokio::time::timeout(timeout, client.chat()).await {
        Err(_) => {
            return Err(ReplayError::new(
                ErrorKind::Timeout,
                format!("{} 秒以内に応答がありませんでした", timeout.as_secs()),
            ));
        }
        Ok(Err(e)) => return Err(classify_llm_error(&e)),
        Ok(Ok(text)) => text,
    };

    let (response, truncated) = truncate_chars(text, MAX_RESPONSE_CHARS);
    Ok(Json(ReplayResponse {
        response,
        model: req.model.trim().to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        truncated,
    }))
}
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: PASS(Task 2 の 14 件 + 本タスクの 13 件)

- [ ] **Step 5: 仕様書の未確認事項を解消する**

`docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md` の「**未確認**: LSP は `build_client` に…」で始まる箇条書き1項目を、次の内容に置き換える:

```markdown
- **システムプロンプト**: LSP の補完・コードアクションは `build_client_async(cfg, "system.md")` でシステムプロンプト `system.md` を付けて呼んでいる。記録される `prompt` はそのユーザー部分なので、再実行も既定で `system.md`(`crate::assets::load("system.md")`)を付ける。リクエストの `system` で上書きでき、空文字ならシステムプロンプトなしにする。`character_updates` や `quality_reviews` など別のシステムプロンプトを使う機能では、元の実行と条件が異なる場合がある(画面の注記に含める)。
```

- [ ] **Step 6: コミット**

```bash
git add lsp/src/replay.rs docs/superpowers/specs/2026-10-09-shinasadame-prompt-trial-design.md
git commit -m "feat(lsp): replay の HTTP ハンドラ・ルーター・ClientFactory を追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 4: サーバーの起動(`--listen`、環境変数、graceful shutdown、ログ)

**Files:**
- Modify: `lsp/src/replay.rs`

**Interfaces:**
- Consumes: `router`、`AppState::new`、`RealFactory`、`DEFAULT_LISTEN`、`DEFAULT_MAX_CONCURRENCY`(Task 2、3)。
- Produces:
  - `pub(crate) fn parse_listen(args: &[String], env_listen: Option<String>) -> Result<SocketAddr, String>`(優先順位: `--listen` > `REPLAY_LISTEN` > 既定)
  - `pub(crate) fn max_concurrency_from(env_value: Option<String>) -> usize`
  - `pub(crate) async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()>`
  - `pub(crate) fn main()`(Task 1 の骨組みを置き換える)

- [ ] **Step 1: 失敗するテストを書く**

`lsp/src/replay.rs` の `mod tests` に追加:

```rust
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_listen_prefers_flag_over_env_over_default() {
        assert_eq!(
            parse_listen(&args(&["--replay", "--listen", "127.0.0.1:9000"]), Some("0.0.0.0:1".into()))
                .unwrap()
                .to_string(),
            "127.0.0.1:9000"
        );
        assert_eq!(
            parse_listen(&args(&["--replay"]), Some("127.0.0.1:9001".into())).unwrap().to_string(),
            "127.0.0.1:9001"
        );
        assert_eq!(
            parse_listen(&args(&["--replay"]), None).unwrap().to_string(),
            DEFAULT_LISTEN
        );
    }

    #[test]
    fn test_parse_listen_rejects_invalid_address_and_missing_value() {
        assert!(parse_listen(&args(&["--replay", "--listen", "not-an-addr"]), None).is_err());
        assert!(parse_listen(&args(&["--replay", "--listen"]), None).is_err());
        assert!(parse_listen(&args(&["--replay"]), Some("bad".into())).is_err());
    }

    #[test]
    fn test_max_concurrency_from_env_value() {
        assert_eq!(max_concurrency_from(None), DEFAULT_MAX_CONCURRENCY);
        assert_eq!(max_concurrency_from(Some("2".into())), 2);
        assert_eq!(max_concurrency_from(Some("0".into())), DEFAULT_MAX_CONCURRENCY);
        assert_eq!(max_concurrency_from(Some("abc".into())), DEFAULT_MAX_CONCURRENCY);
    }

    #[tokio::test]
    async fn test_serve_answers_over_real_http_and_stops_on_shutdown() {
        let f = StubFactory::new(Behavior::Reply("本物のHTTP".to_string()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, AppState::new(f, 2), async {
            let _ = rx.await;
        }));

        let client = reqwest::Client::new();
        let health = client.get(format!("http://{addr}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), 200);
        let res = client
            .post(format!("http://{addr}/v1/replay"))
            .json(&serde_json::json!({"model": "google/gemini-3.1-pro", "prompt": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<serde_json::Value>().await.unwrap()["response"], "本物のHTTP");

        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown で止まる")
            .unwrap()
            .unwrap();
    }
```

- [ ] **Step 2: 失敗を確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: FAIL(コンパイルエラー: `parse_listen`、`max_concurrency_from`、`serve` が未定義)

- [ ] **Step 3: 実装する**

`lsp/src/replay.rs` の `use` 群に追加:

```rust
use std::future::Future;
use std::net::SocketAddr;
```

Task 1 の `main()` を削除し、`replay_handler` の後に次を追加:

```rust
/// 待ち受けアドレスを決める。優先順位: `--listen` > 環境変数 `REPLAY_LISTEN` > 既定。
pub(crate) fn parse_listen(args: &[String], env_listen: Option<String>) -> Result<SocketAddr, String> {
    let from_flag = match args.iter().position(|a| a == "--listen") {
        Some(i) => Some(
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| "--listen にアドレスがありません".to_string())?,
        ),
        None => None,
    };
    let raw = from_flag
        .or(env_listen)
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    raw.parse::<SocketAddr>()
        .map_err(|e| format!("待ち受けアドレスが不正です ({raw}): {e}"))
}

/// `REPLAY_MAX_CONCURRENCY` の値から同時実行数を決める。0 や数値でないものは既定に戻す。
pub(crate) fn max_concurrency_from(env_value: Option<String>) -> usize {
    env_value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
}

/// `shutdown` が完了したら、処理中のリクエストを待ってから止まる。
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Ctrl+C または SIGTERM(k8s の Pod 停止)を待つ。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// `--replay` のエントリポイント。標準出力は使わない(ログは stderr / OTLP)。
pub(crate) fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = match parse_listen(&args, std::env::var("REPLAY_LISTEN").ok()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fifty_four_lsp --replay: {e}");
            std::process::exit(2);
        }
    };
    let max = max_concurrency_from(std::env::var("REPLAY_MAX_CONCURRENCY").ok());
    run(addr, max);
}

#[tokio::main]
async fn run(addr: SocketAddr, max_concurrency: usize) {
    // LSP/ACP と同じロガー。service.name の区別は acp フラグで行うため、ここでは false。
    let _log = crate::logging::Logger::new(false);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fifty_four_lsp --replay: {addr} で待ち受けできません: {e}");
            std::process::exit(1);
        }
    };
    log::info!("replay server listening on {addr} (max_concurrency={max_concurrency})");

    let state = AppState::new(Arc::new(RealFactory), max_concurrency);
    if let Err(e) = serve(listener, state, shutdown_signal()).await {
        eprintln!("fifty_four_lsp --replay: {e}");
        drop(_log);
        std::process::exit(1);
    }
    log::info!("replay server stopped");
}
```

`crate::logging::Logger` の可視性が足りない場合(`mod logging;` は `main.rs` 内の同階層なので `crate::logging::Logger` が見える。`Logger::new` は `pub fn`)はそのまま使える。

- [ ] **Step 4: テストが通ることを確認する**

Run: `cd lsp && cargo test --features replay replay::tests`
Expected: PASS(これまでの 27 件 + 本タスクの 4 件)

- [ ] **Step 5: 実際に起動して確かめる**

Run(別ターミナル、またはバックグラウンド): `cd lsp && cargo run --features replay -- --replay --listen 127.0.0.1:18080`

別のコマンドで:

```bash
curl -s http://127.0.0.1:18080/healthz
curl -s -X POST http://127.0.0.1:18080/v1/replay -H 'content-type: application/json' -d '{"model":"nosuch/m","prompt":"x"}'
```

Expected: `ok` と、`{"error":{"kind":"bad_request","message":"Unsupported provider: nosuch/m"}}`。確認後、起動したプロセスを必ず終了させる(ユーザーのメモリの規則: `fifty_four_lsp.exe` を残さない)。

```bash
# Windows
taskkill /IM fifty_four_lsp.exe /F
```

標準出力に何も出ていないこと(ログは stderr)も確認する。

- [ ] **Step 6: 既定 feature のビルドとテストが壊れていないことを確認する**

Run: `cd lsp && cargo test`
Expected: PASS(既存の 539 件 + `replay_flag` の結合テスト 1 件。`replay` のテストは feature 無効なので走らない)

- [ ] **Step 7: コミット**

```bash
git add lsp/src/replay.rs
git commit -m "feat(lsp): replay サーバーの起動・graceful shutdown・設定を追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 5: CI に `--features replay` のテストを追加

**Files:**
- Modify: `.github/workflows/test.yml`

**Interfaces:**
- Consumes: Task 1〜4 で追加したテスト。

- [ ] **Step 1: ワークフローを確認する**

Run: `cat .github/workflows/test.yml`
`lsp` ジョブ(`name: cargo test (lsp)`)の `run: cargo test -p fifty_four_lsp` を確認する。

- [ ] **Step 2: ステップを追加する**

`lsp` ジョブの `- run: cargo test -p fifty_four_lsp` の直後に追加:

```yaml
      - run: cargo test -p fifty_four_lsp --features replay
```

(ブランチ保護で必須にしているチェック名 `cargo test (lsp)` は変えない。ステップを足すだけなので名前は変わらない)

- [ ] **Step 3: 構文を確認する**

Run: `git diff .github/workflows/test.yml`
Expected: 1 行の追加のみ。インデントが他のステップと揃っている。

- [ ] **Step 4: コミット**

```bash
git add .github/workflows/test.yml
git commit -m "ci: lsp ジョブで --features replay のテストも実行する

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 6: shinasadame の ReplayClient と設定

**Files:**
- Modify: `shinasadame/pyproject.toml`、`shinasadame/uv.lock`
- Modify: `shinasadame/src/shinasadame/config.py`
- Create: `shinasadame/src/shinasadame/replay.py`
- Create: `shinasadame/tests/test_replay_client.py`

**Interfaces:**
- Produces:
  - `config.replay_url() -> str`(既定 `http://replay:8080`)、`config.replay_default_timeout_secs() -> int`(既定 120)、`config.model_choices() -> list[str]`(`SHINASADAME_MODELS` をカンマ区切りで分割。空要素を除く)
  - `replay.ReplayOk(response: str, model: str, elapsed_ms: int, truncated: bool)`
  - `replay.ReplayFailed(kind: str, message: str)`
  - `replay.ReplayUnavailable(Exception)`
  - `replay.ReplayClient(base_url: str | None = None, transport: httpx.AsyncBaseTransport | None = None)` と `async def run(self, model: str, prompt: str, timeout_secs: int | None = None) -> ReplayOk | ReplayFailed`(接続不能・タイムアウト・想定外の応答は `ReplayUnavailable` を送出)、`async def aclose(self)`

- [ ] **Step 1: `httpx` を実行時依存へ移し、async テストの環境を整える**

`shinasadame/pyproject.toml`:
- `dependencies` に `"httpx>=0.27",` を追加し、`[dependency-groups] dev` から `"httpx>=0.27",` を削除する。
- `[dependency-groups] dev` に `"pytest-asyncio>=0.24",` を追加する(現状 dev 依存は `pytest` だけで、`async def` のテストは動かない)。
- `[tool.pytest.ini_options]` に `asyncio_mode = "auto"` を追加する。

Run: `cd shinasadame && uv lock`
Expected: `uv.lock` が更新される。

- [ ] **Step 2: 失敗するテストを書く**

`shinasadame/tests/test_replay_client.py`:

```python
import httpx
import pytest

from shinasadame import config
from shinasadame.replay import ReplayClient, ReplayFailed, ReplayOk, ReplayUnavailable


def client_with(handler) -> ReplayClient:
    return ReplayClient(base_url="http://replay.test", transport=httpx.MockTransport(handler))


async def test_run_returns_ok_and_sends_expected_request():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["path"] = request.url.path
        seen["body"] = request.read()
        return httpx.Response(
            200, json={"response": "返答", "model": "m", "elapsed_ms": 12, "truncated": False}
        )

    result = await client_with(handler).run("google/gemini-3.1-pro", "続きを書いて", timeout_secs=30)

    assert result == ReplayOk(response="返答", model="m", elapsed_ms=12, truncated=False)
    assert seen["path"] == "/v1/replay"
    import json

    assert json.loads(seen["body"]) == {
        "model": "google/gemini-3.1-pro",
        "prompt": "続きを書いて",
        "timeout_secs": 30,
    }


async def test_run_uses_default_timeout_when_not_given(monkeypatch):
    monkeypatch.setenv("REPLAY_DEFAULT_TIMEOUT_SECS", "45")
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        import json

        seen["timeout_secs"] = json.loads(request.read())["timeout_secs"]
        return httpx.Response(200, json={"response": "", "model": "m", "elapsed_ms": 1, "truncated": False})

    await client_with(handler).run("google/x", "p")
    assert seen["timeout_secs"] == 45


@pytest.mark.parametrize(
    ("status", "kind"),
    [(400, "bad_request"), (502, "auth"), (502, "llm"), (504, "timeout"), (500, "internal")],
)
async def test_run_maps_error_json_to_failed(status, kind):
    def handler(_request):
        return httpx.Response(status, json={"error": {"kind": kind, "message": "理由"}})

    result = await client_with(handler).run("google/x", "p")
    assert result == ReplayFailed(kind=kind, message="理由")


async def test_run_raises_unavailable_when_connection_fails():
    def handler(_request):
        raise httpx.ConnectError("refused")

    with pytest.raises(ReplayUnavailable):
        await client_with(handler).run("google/x", "p")


async def test_run_raises_unavailable_on_client_timeout():
    def handler(_request):
        raise httpx.ReadTimeout("slow")

    with pytest.raises(ReplayUnavailable):
        await client_with(handler).run("google/x", "p")


async def test_run_raises_unavailable_on_non_json_body():
    def handler(_request):
        return httpx.Response(502, text="<html>Bad Gateway</html>")

    with pytest.raises(ReplayUnavailable) as e:
        await client_with(handler).run("google/x", "p")
    assert "502" in str(e.value)


def test_config_defaults_and_model_choices(monkeypatch):
    monkeypatch.delenv("REPLAY_URL", raising=False)
    monkeypatch.delenv("REPLAY_DEFAULT_TIMEOUT_SECS", raising=False)
    monkeypatch.delenv("SHINASADAME_MODELS", raising=False)
    assert config.replay_url() == "http://replay:8080"
    assert config.replay_default_timeout_secs() == 120
    assert config.model_choices() == []

    monkeypatch.setenv("SHINASADAME_MODELS", " google/gemini-3.1-pro ,, anthropic/claude-sonnet-5-5 ,")
    assert config.model_choices() == ["google/gemini-3.1-pro", "anthropic/claude-sonnet-5-5"]


def test_config_ignores_invalid_timeout(monkeypatch):
    monkeypatch.setenv("REPLAY_DEFAULT_TIMEOUT_SECS", "abc")
    assert config.replay_default_timeout_secs() == 120
```

- [ ] **Step 3: 失敗を確認する**

Run: `cd shinasadame && uv run pytest tests/test_replay_client.py -q`
Expected: FAIL(`shinasadame.replay` が無い)

- [ ] **Step 4: 実装する**

`shinasadame/src/shinasadame/config.py` の末尾に追加:

```python
def replay_url() -> str:
    return os.environ.get("REPLAY_URL", "http://replay:8080").rstrip("/")


def replay_default_timeout_secs() -> int:
    try:
        value = int(os.environ.get("REPLAY_DEFAULT_TIMEOUT_SECS", "120"))
    except ValueError:
        return 120
    return value if value > 0 else 120


def model_choices() -> list[str]:
    """画面の選択肢にする `provider/model`。`SHINASADAME_MODELS` をカンマ区切りで読む。"""
    raw = os.environ.get("SHINASADAME_MODELS", "")
    return [m.strip() for m in raw.split(",") if m.strip()]
```

`shinasadame/src/shinasadame/replay.py`:

```python
"""replay サービス(`fifty_four_lsp --replay`)の HTTP クライアント。

HTTP ステータスの解釈はここに閉じ込める。呼び出し側は `ReplayOk` / `ReplayFailed` を見て、
サービスに届かなかったときだけ `ReplayUnavailable` を受け取る。
"""

from dataclasses import dataclass

import httpx

from . import config


class ReplayUnavailable(Exception):
    """replay サービスに接続できない、または想定外の応答が返った。"""


@dataclass(frozen=True)
class ReplayOk:
    response: str
    model: str
    elapsed_ms: int
    truncated: bool


@dataclass(frozen=True)
class ReplayFailed:
    """replay サービスが返したエラー。kind は bad_request / auth / timeout / llm / internal。"""

    kind: str
    message: str


class ReplayClient:
    def __init__(
        self,
        base_url: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self._client = httpx.AsyncClient(base_url=base_url or config.replay_url(), transport=transport)

    async def aclose(self) -> None:
        await self._client.aclose()

    async def run(
        self, model: str, prompt: str, timeout_secs: int | None = None
    ) -> ReplayOk | ReplayFailed:
        timeout = timeout_secs or config.replay_default_timeout_secs()
        try:
            res = await self._client.post(
                "/v1/replay",
                json={"model": model, "prompt": prompt, "timeout_secs": timeout},
                # サーバー側のタイムアウトより少し長く待つ(サーバーの 504 を受け取れるように)
                timeout=timeout + 10,
            )
        except httpx.HTTPError as e:
            raise ReplayUnavailable(f"replay サービスに接続できません: {e!r}") from e

        try:
            body = res.json()
        except ValueError:
            raise ReplayUnavailable(f"replay サービスの応答を解釈できません (HTTP {res.status_code})") from None

        if res.status_code == 200 and isinstance(body, dict) and "response" in body:
            return ReplayOk(
                response=body["response"],
                model=body.get("model", model),
                elapsed_ms=int(body.get("elapsed_ms", 0)),
                truncated=bool(body.get("truncated", False)),
            )
        error = body.get("error") if isinstance(body, dict) else None
        if isinstance(error, dict) and "kind" in error:
            return ReplayFailed(kind=str(error["kind"]), message=str(error.get("message", "")))
        raise ReplayUnavailable(f"replay サービスの応答が想定外です (HTTP {res.status_code})")
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cd shinasadame && uv run pytest tests/test_replay_client.py -q`
Expected: PASS(全件。パラメータ化を含め 15 件前後)

Run: `cd shinasadame && uv run pytest -q`
Expected: 既存の 52 件も含めて全件 PASS

- [ ] **Step 6: コミット**

```bash
git add shinasadame/pyproject.toml shinasadame/uv.lock shinasadame/src/shinasadame/config.py shinasadame/src/shinasadame/replay.py shinasadame/tests/test_replay_client.py
git commit -m "feat(shinasadame): replay サービスの HTTP クライアントと設定を追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 7: 「試す」エンドポイントと画面

**Files:**
- Modify: `shinasadame/src/shinasadame/records.py`
- Modify: `shinasadame/src/shinasadame/app.py`
- Create: `shinasadame/src/shinasadame/templates/_trial_result.html`
- Modify: `shinasadame/src/shinasadame/templates/record_detail.html`
- Create: `shinasadame/tests/test_trial.py`

**Interfaces:**
- Consumes: Task 6 の `ReplayClient`、`ReplayOk`、`ReplayFailed`、`ReplayUnavailable`、`config.model_choices()`。
- Produces:
  - `records.PROMPT_TABLES: frozenset[str]`(`prompt` 列を持つテーブル名)
  - `records.original_output(detail: dict) -> str | None`(元の出力。`response` 列があればそれ、無ければ候補を連結)
  - `app.get_replay_client() -> ReplayClient`(FastAPI の依存関係。テストは `app.dependency_overrides` で差し替える)
  - `POST /s/{sid}/records/{table}/{record_id}/try`(フォーム `prompt`, `model`)

- [ ] **Step 1: 失敗するテストを書く**

`shinasadame/tests/test_trial.py`:

```python
import pytest
from fastapi.testclient import TestClient
from sqlalchemy import select

from shinasadame import db, etl, records
from shinasadame.app import app, get_replay_client
from shinasadame.replay import ReplayFailed, ReplayOk, ReplayUnavailable

import fr_db


class FakeReplay:
    def __init__(self, result=None, error=None):
        self.result = result
        self.error = error
        self.calls = []

    async def run(self, model, prompt, timeout_secs=None):
        self.calls.append((model, prompt))
        if self.error:
            raise self.error
        return self.result


@pytest.fixture
def client():
    with TestClient(app) as c:
        yield c
    app.dependency_overrides.clear()


@pytest.fixture
def sid(engine, fr_v5):
    return etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))


def use(fake):
    app.dependency_overrides[get_replay_client] = lambda: fake
    return fake


OK = ReplayOk(response="試行の応答", model="google/gemini-3.1-pro", elapsed_ms=1234, truncated=False)


def test_prompt_tables_are_derived_from_columns():
    assert records.PROMPT_TABLES == {"completions", "code_actions", "character_updates", "quality_reviews"}


def test_original_output_uses_response_or_joined_candidates(engine, sid):
    ca = records.get_record(engine, sid, "code_actions", 1)
    assert records.original_output(ca) == '{"candidates":[]}'
    comp = records.get_record(engine, sid, "completions", 1)
    assert records.original_output(comp) == "採用された続き\n---\n別案その一\n---\n別案その二"
    finding = records.get_record(engine, sid, "quality_findings", 1)
    assert records.original_output(finding) is None


def test_detail_page_shows_try_panel_with_original_prompt_and_note(client, sid, monkeypatch):
    monkeypatch.setenv("SHINASADAME_MODELS", "google/gemini-3.1-pro,anthropic/claude-sonnet-5-5")
    page = client.get(f"/s/{sid}/records/completions/1").text

    assert f'hx-post="/s/{sid}/records/completions/1/try"' in page
    assert "prompt one" in page  # 元のプロンプトが編集欄の初期値
    assert 'value="model-a"' in page  # 記録の model_name が既定値
    assert "google/gemini-3.1-pro" in page and "anthropic/claude-sonnet-5-5" in page
    assert "tool なしで実行します" in page


def test_detail_page_hides_try_panel_for_tables_without_prompt(client, sid):
    page = client.get(f"/s/{sid}/records/quality_findings/1").text
    assert "/try" not in page


def test_try_shows_original_and_trial_side_by_side(client, sid):
    fake = use(FakeReplay(result=OK))
    res = client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "編集後のプロンプト", "model": "google/gemini-3.1-pro"},
    )

    assert res.status_code == 200
    assert fake.calls == [("google/gemini-3.1-pro", "編集後のプロンプト")]
    assert "試行の応答" in res.text
    assert '{&#34;candidates&#34;:[]}' in res.text or '{"candidates":[]}' in res.text  # 元の応答
    assert "1234" in res.text  # elapsed_ms
    assert "google/gemini-3.1-pro" in res.text


def test_try_escapes_html_in_llm_output(client, sid):
    use(FakeReplay(result=ReplayOk("<script>alert(1)</script>", "m", 1, False)))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert "<script>alert(1)</script>" not in res.text
    assert "&lt;script&gt;" in res.text


def test_try_notes_truncation(client, sid):
    use(FakeReplay(result=ReplayOk("長い応答", "m", 1, True)))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert "切り詰め" in res.text


@pytest.mark.parametrize(
    ("kind", "needle"),
    [
        ("bad_request", "リクエストが不正です"),
        ("auth", "認証に失敗しました"),
        ("timeout", "タイムアウト"),
        ("llm", "LLM がエラーを返しました"),
        ("internal", "replay サービスの内部エラー"),
    ],
)
def test_try_shows_error_kinds_with_http_200(client, sid, kind, needle):
    use(FakeReplay(result=ReplayFailed(kind=kind, message="詳細メッセージ")))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert res.status_code == 200  # htmx がスワップできるよう 200
    assert needle in res.text
    assert "詳細メッセージ" in res.text


def test_try_when_replay_is_down_does_not_500(client, sid):
    use(FakeReplay(error=ReplayUnavailable("refused")))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert res.status_code == 200
    assert "replay サービスに接続できません" in res.text


def test_try_unknown_table_record_and_snapshot_are_404(client, sid):
    use(FakeReplay(result=OK))
    form = {"prompt": "p", "model": "google/x"}
    assert client.post(f"/s/{sid}/records/quality_findings/1/try", data=form).status_code == 404
    assert client.post(f"/s/{sid}/records/nosuch/1/try", data=form).status_code == 404
    assert client.post(f"/s/{sid}/records/completions/999/try", data=form).status_code == 404
    assert client.post("/s/999/records/completions/1/try", data=form).status_code == 404


def test_try_requires_prompt_and_model(client, sid):
    use(FakeReplay(result=OK))
    assert client.post(f"/s/{sid}/records/completions/1/try", data={"model": "google/x"}).status_code == 422
    assert client.post(f"/s/{sid}/records/completions/1/try", data={"prompt": "p"}).status_code == 422


def test_edit_area_escapes_closing_textarea(client, sid, engine):
    with db.get_engine().begin() as conn:
        conn.execute(
            db.FR_TABLES["completions"]
            .update()
            .where(db.FR_TABLES["completions"].c.snapshot_id == sid, db.FR_TABLES["completions"].c.id == 1)
            .values(prompt="前</textarea><b>後")
        )
    page = client.get(f"/s/{sid}/records/completions/1").text
    assert "</textarea><b>" not in page
    assert "&lt;/textarea&gt;" in page
```

- [ ] **Step 2: 失敗を確認する**

Run: `cd shinasadame && uv run pytest tests/test_trial.py -q`
Expected: FAIL(`get_replay_client` が import できない)

- [ ] **Step 3: `records.py` に `PROMPT_TABLES` と `original_output` を足す**

`shinasadame/src/shinasadame/records.py` の `SPECS` の定義より後に追加:

```python
# プロンプトを持つテーブル。画面の「試す」パネルの対象。
PROMPT_TABLES = frozenset(name for name, t in FR_TABLES.items() if "prompt" in t.c)


def original_output(detail: dict) -> str | None:
    """元の実行の出力。`response` 列があればそれ、無ければ(補完)候補を順位順に連結する。"""
    row = detail["row"]
    if row.get("response") is not None:
        return row["response"]
    for _label, child_table, items in detail["children"]:
        if child_table.endswith("_candidates") and items:
            return "\n---\n".join(it["candidate"] for it in items)
    return None
```

- [ ] **Step 4: `app.py` にエンドポイントと依存関係を足す**

`from fastapi import ...` の行に `Depends` を足し、`from . import config, db, etl, records, tasks` の後に追加:

```python
from functools import lru_cache

from .replay import ReplayClient, ReplayFailed, ReplayUnavailable
```

`templates.env.filters[...]` の後に追加:

```python
@lru_cache
def get_replay_client() -> ReplayClient:
    """FastAPI の依存関係。接続を使い回すため 1 つだけ作る。テストは dependency_overrides で差し替える。"""
    return ReplayClient()
```

`lifespan` を、終了時に使い回しのクライアントを閉じるよう変更する(`get_replay_client` を一度でも呼んだときだけ):

```python
@asynccontextmanager
async def lifespan(_app: FastAPI):
    db.init_db()
    yield
    if get_replay_client.cache_info().currsize:
        await get_replay_client().aclose()
```

`record_detail` を次のように変更(コンテキストに `can_try`、`model_choices` を足す):

```python
@app.get("/s/{sid}/records/{table}/{record_id}")
def record_detail(request: Request, sid: int, table: str, record_id: int):
    snapshot = _snapshot_or_404(sid)
    spec = _spec_or_404(table)
    detail = records.get_record(db.get_engine(), sid, table, record_id)
    if detail is None:
        raise HTTPException(404, "レコードが見つかりません")
    return _render(
        request,
        "record_detail.html",
        sid=sid,
        snapshot=snapshot,
        table=table,
        spec=spec,
        record_id=record_id,
        can_try=table in records.PROMPT_TABLES,
        model_choices=config.model_choices(),
        **detail,
    )
```

`record_detail` の後に追加:

```python
@app.post("/s/{sid}/records/{table}/{record_id}/try")
async def try_prompt(
    request: Request,
    sid: int,
    table: str,
    record_id: int,
    prompt: str = Form(...),
    model: str = Form(...),
    client: ReplayClient = Depends(get_replay_client),
):
    """編集したプロンプトを replay サービスに投げ、元の出力と並べる部分テンプレートを返す。

    エラーも HTTP 200 で返す(htmx は 4xx/5xx の本文をスワップしないため)。
    """
    _snapshot_or_404(sid)
    _spec_or_404(table)
    if table not in records.PROMPT_TABLES:
        raise HTTPException(404, "このテーブルにはプロンプトがありません")
    detail = records.get_record(db.get_engine(), sid, table, record_id)
    if detail is None:
        raise HTTPException(404, "レコードが見つかりません")

    model = model.strip()
    try:
        result = await client.run(model, prompt)
    except ReplayUnavailable as e:
        result = ReplayFailed(kind="unavailable", message=str(e))
    return templates.TemplateResponse(
        request,
        "_trial_result.html",
        {"original": records.original_output(detail), "result": result, "model": model},
    )
```

- [ ] **Step 5: テンプレートを書く**

`shinasadame/src/shinasadame/templates/_trial_result.html`:

```html
{# 試行結果。result は ReplayOk か ReplayFailed。HTTP は常に 200(htmx がスワップできるように) #}
{% set error_labels = {
  'bad_request': 'リクエストが不正です',
  'auth': '認証に失敗しました',
  'timeout': 'タイムアウトしました',
  'llm': 'LLM がエラーを返しました',
  'internal': 'replay サービスの内部エラーです',
  'unavailable': 'replay サービスに接続できません',
} %}
{% if result.kind is defined %}
  <div class="trial-error">
    <strong>{{ error_labels.get(result.kind, '失敗しました') }}</strong>
    <pre>{{ result.message }}</pre>
  </div>
{% else %}
  <p class="muted">{{ result.model }} · {{ result.elapsed_ms }} ms</p>
  {% if result.truncated %}<p class="muted">応答が長いため途中で切り詰めました</p>{% endif %}
  <div class="trial-compare">
    <div>
      <h3>元の出力</h3>
      {% if original is none %}<p class="muted">—</p>{% else %}<pre>{{ original }}</pre>{% endif %}
    </div>
    <div>
      <h3>試行結果</h3>
      <pre>{{ result.response }}</pre>
    </div>
  </div>
{% endif %}
```

`shinasadame/src/shinasadame/templates/record_detail.html` の `</dl>` の直後に追加:

```html
{% if can_try %}
<h2>試す</h2>
<form class="trial" hx-post="/s/{{ sid }}/records/{{ table }}/{{ record_id }}/try"
      hx-target="#trial-result" hx-disabled-elt="find button" hx-indicator="#trial-spinner">
  <p class="muted">tool なしで実行します。元の実行とは条件が異なります。</p>
  <label>プロンプト
    <textarea name="prompt" rows="12" required>{{ row.prompt }}</textarea>
  </label>
  <label>モデル
    <input name="model" list="trial-models" value="{{ row.model_name }}" required>
    <datalist id="trial-models">
      {% for m in model_choices %}<option value="{{ m }}">{% endfor %}
    </datalist>
  </label>
  <button type="submit">試す</button>
  <span id="trial-spinner" class="htmx-indicator muted">実行中…</span>
</form>
<div id="trial-result"></div>
{% endif %}
```

`shinasadame/src/shinasadame/static/style.css` の末尾に追加:

```css
.trial textarea { width: 100%; font-family: monospace; }
.trial label { display: block; margin: .5rem 0; }
.trial-compare { display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; }
.trial-compare pre { white-space: pre-wrap; }
.trial-error { border-left: 4px solid #c33; padding-left: .75rem; }
.htmx-indicator { display: none; }
.htmx-request .htmx-indicator, .htmx-request.htmx-indicator { display: inline; }
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cd shinasadame && uv run pytest tests/test_trial.py -q`
Expected: PASS(全件)

`'value="model-a"'` の検証が失敗する場合は、`row.model_name` の値の出力が `value="model-a"` になっているか(`record_detail.html` の `<input>` の属性順)を確認する。`code_actions` の元の応答の検証は、Jinja2 のエスケープ(`&#34;`)の有無で `'{"candidates":[]}'` の表記が変わるため、両方を許す形にしてある。

Run: `cd shinasadame && uv run pytest -q`
Expected: 全件 PASS(既存 52 件 + `test_replay_client` + `test_trial`)

- [ ] **Step 7: ブラウザで確認する(任意だが推奨)**

Run: `cd shinasadame && uv run uvicorn shinasadame.app:app --port 8000`(停止は Ctrl+C。確認後は必ず終了させる)

`REPLAY_URL` を未設定のまま画面の「試す」を押し、**500 にならず**「replay サービスに接続できません」が出ることを確認する。

- [ ] **Step 8: コミット**

```bash
git add shinasadame/src/shinasadame/records.py shinasadame/src/shinasadame/app.py shinasadame/src/shinasadame/templates/_trial_result.html shinasadame/src/shinasadame/templates/record_detail.html shinasadame/src/shinasadame/static/style.css shinasadame/tests/test_trial.py
git commit -m "feat(shinasadame): レコード詳細に「試す」パネルと /try エンドポイントを追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 8: イメージと k8s マニフェスト

**Files:**
- Create: `lsp/Dockerfile.replay`
- Create: `.dockerignore`(リポジトリルート。無ければ)
- Create: `shinasadame/k8s/base/replay-deployment.yaml`
- Create: `shinasadame/k8s/base/replay-service.yaml`
- Modify: `shinasadame/k8s/base/kustomization.yaml`
- Modify: `shinasadame/k8s/base/deployment.yaml`(web コンテナに `REPLAY_URL` と `SHINASADAME_MODELS`)
- Modify: `shinasadame/README.md`(replay のビルド・Secret・配備の手順)

**Interfaces:**
- Consumes: Task 4 の `--replay`(`REPLAY_LISTEN`、`REPLAY_MAX_CONCURRENCY`、`/healthz`)、Task 6 の `REPLAY_URL`、`SHINASADAME_MODELS`。

- [ ] **Step 1: `.dockerignore` を作る**

リポジトリルートに `.dockerignore`(既にある場合は内容を確認して追記):

```
target/
**/target/
.git/
.env*
.claude/
.idea/
dist/
db/
shinasadame/.venv/
shinasadame/.data/
**/__pycache__/
```

- [ ] **Step 2: `lsp/Dockerfile.replay` を書く**

```dockerfile
# replay サービス(fifty_four_lsp --replay)用のイメージ。
# ビルドコンテキストはリポジトリルート:
#   docker build -f lsp/Dockerfile.replay -t fifty-four-replay:latest .
FROM rust:1-bookworm AS build
WORKDIR /src

# 依存だけ先にビルドしてレイヤーキャッシュを効かせる。
# lindera の辞書埋め込み(embed-ipadic)と rusqlite(bundled)で初回は重い。
COPY Cargo.toml Cargo.lock* ./
COPY lsp/Cargo.toml lsp/Cargo.toml
COPY extension/Cargo.toml extension/Cargo.toml
COPY prepare/Cargo.toml prepare/Cargo.toml
RUN mkdir -p lsp/src extension/src prepare/src \
    && echo 'fn main() {}' > lsp/src/main.rs \
    && echo '' > extension/src/lib.rs \
    && echo 'fn main() {}' > prepare/src/main.rs \
    && cargo build --release -p fifty_four_lsp --features replay || true

COPY lsp ./lsp
COPY extension ./extension
COPY prepare ./prepare
# 依存のダミービルドより新しいと判定されるよう、ソースの更新時刻を進める
RUN touch lsp/src/main.rs \
    && cargo build --release -p fifty_four_lsp --features replay

FROM debian:bookworm-slim
# 各 provider へ HTTPS で接続するために CA 証明書が要る
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --no-create-home replay
COPY --from=build /src/target/release/fifty_four_lsp /usr/local/bin/fifty_four_lsp
USER 10001
ENV REPLAY_LISTEN=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/fifty_four_lsp", "--replay"]
```

ワークスペースの構成は `lsp`(bin)、`extension`(`src/lib.rs`、cdylib)、`prepare`(`src/main.rs`)なので、ダミーの置き方は上のとおり。ダミービルドが失敗しても `|| true` で続行するが、最終ビルドは失敗させない。`prepare` や `extension` が依存を増やしてダミービルドが通らなくなった場合は、`COPY` する Cargo.toml と ダミーのソースを実ファイルに合わせて直す。

- [ ] **Step 3: イメージをビルドして起動を確かめる**

Run: `docker build -f lsp/Dockerfile.replay -t fifty-four-replay:latest .`
Expected: 成功(初回は十数分かかることがある)。失敗した場合は、エラーを読んで Dockerfile を直す。

Run: `docker run --rm -d --name replay-test -p 18080:8080 fifty-four-replay:latest`
Run: `curl -s http://127.0.0.1:18080/healthz`
Expected: `ok`

Run: `curl -s -X POST http://127.0.0.1:18080/v1/replay -H 'content-type: application/json' -d '{"model":"google/gemini-3.1-pro","prompt":"x"}'`
Expected: API キー未設定なので `{"error":{"kind":"auth",...}}` または `llm`(どちらかで、500 や接続切れでないこと)。この結果が `auth` になるかを記録する(ならない場合は Task 2 の `AUTH_HINTS` に、実際のメッセージの特徴的な語を足し、テストを追加する)。

Run: `docker rm -f replay-test`

- [ ] **Step 4: k8s マニフェストを書く**

`shinasadame/k8s/base/replay-deployment.yaml`:

```yaml
# replay サービス(fifty_four_lsp --replay)。shinasadame の Pod とは別の Pod にする。
# API キーはこの Pod だけが持つ。Secret `replay-keys` は手動で作る(README 参照)。
apiVersion: apps/v1
kind: Deployment
metadata:
  name: replay
spec:
  replicas: 1
  selector:
    matchLabels:
      app: replay
  template:
    metadata:
      labels:
        app: replay
    spec:
      containers:
        - name: replay
          image: fifty-four-replay:latest
          # docker build したイメージを registry-mirror 経由で pull する
          imagePullPolicy: IfNotPresent
          ports:
            - containerPort: 8080
          env:
            - name: REPLAY_MAX_CONCURRENCY
              value: "4"
            - name: OTEL_EXPORTER_OTLP_ENDPOINT
              value: http://localhost:4317
          envFrom:
            - secretRef:
                name: replay-keys
                optional: true # 未作成でも起動する(その provider は auth エラーになる)
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8080
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
            initialDelaySeconds: 10
```

`shinasadame/k8s/base/replay-service.yaml`:

```yaml
# クラスタ内からだけ届く。外部には公開しない。
apiVersion: v1
kind: Service
metadata:
  name: replay
spec:
  type: ClusterIP
  selector:
    app: replay
  ports:
    - port: 8080
      targetPort: 8080
```

`shinasadame/k8s/base/kustomization.yaml` の `resources` に追加:

```yaml
  - replay-deployment.yaml
  - replay-service.yaml
```

`shinasadame/k8s/base/deployment.yaml` の **web コンテナ**の `env` に追加(worker には足さない):

```yaml
            - name: REPLAY_URL
              value: http://replay:8080
            - name: SHINASADAME_MODELS
              value: "google/gemini-3.1-pro,anthropic/claude-sonnet-5-5,xai/grok-4.7"
```

(モデル名の例は `lsp/src/llm.rs` の `Provider::from_str` の既定値に合わせている。使うものに書き換える。`lmstudio@host.docker.internal:1234/<model>` を使うなら、ここに足す)

- [ ] **Step 5: マニフェストの構文を確認する**

Run: `kubectl kustomize shinasadame/k8s`
Expected: エラーなく YAML が出力され、`replay` の Deployment と Service、web コンテナの `REPLAY_URL` が含まれる。`kubectl` が無い場合は、その旨を記録して次に進む。

- [ ] **Step 6: README に手順を書く**

`shinasadame/README.md` に「プロンプト試行(replay サービス)」の節を追加:

```markdown
## プロンプト試行(replay サービス)

レコード詳細の「試す」パネルは、別 Pod の `replay` サービス(`fifty_four_lsp --replay`)を呼ぶ。

1. イメージをビルドする(リポジトリルートで):

   ```bash
   docker build -f lsp/Dockerfile.replay -t fifty-four-replay:latest .
   ```

2. API キーの Secret を作る(使う provider の分だけ。replay の Pod だけがこれを読む):

   ```bash
   kubectl -n shinasadame create secret generic replay-keys \
     --from-literal=GEMINI_API_KEY=... \
     --from-literal=ANTHROPIC_API_KEY=... \
     --from-literal=OPENAI_API_KEY=... \
     --from-literal=XAI_API_KEY=...
   ```

3. 配備する: `kubectl apply -k shinasadame/k8s`

- 選択肢にするモデルは web コンテナの環境変数 `SHINASADAME_MODELS`(カンマ区切りの `provider/model`)。画面では自由入力もできる。
- LM Studio(ホスト)は `lmstudio@host.docker.internal:1234/<model>` で指定する。
- tool(`character_info` / `plot_info`)は渡さない。元の実行とは条件が異なる。
- 試行結果は保存しない。
```

- [ ] **Step 7: コミット**

```bash
git add .dockerignore lsp/Dockerfile.replay shinasadame/k8s shinasadame/README.md
git commit -m "feat: replay サービスのイメージと k8s マニフェストを追加

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 9: 通しの確認

**Files:** なし(確認のみ。問題があれば該当タスクのファイルを直す)

- [ ] **Step 1: すべてのテストを走らせる**

Run: `cd lsp && cargo test && cargo test --features replay`
Expected: どちらも PASS、警告なし。

Run: `cd shinasadame && uv run pytest -q`
Expected: 全件 PASS。

- [ ] **Step 2: 通常ビルドに `axum` が混ざっていないことを確認する**

Run: `cd lsp && cargo tree -e normal | grep -c "axum"`
Expected: `0`(feature なしでは依存に出ない)

Run: `cd lsp && cargo tree -e normal --features replay | grep -c "axum"`
Expected: 1 以上。

- [ ] **Step 3: 実機で通す(API キーを使える場合)**

1. `kubectl apply -k shinasadame/k8s` で配備し、`kubectl -n shinasadame get pods` で `shinasadame` と `replay` が Ready になるのを確認する。
2. `http://localhost:8054` を開き、FlightRecorder の DB を取り込んで、レコード詳細の「試す」を押す。
3. 元の出力と試行結果が並ぶこと、replay の Pod を `kubectl -n shinasadame scale deploy/replay --replicas=0` で止めると「replay サービスに接続できません」が出て、戻すと復帰することを確認する。

API キーが無い・クラスタが無い場合は、確認できなかった範囲を最終報告に明記する。

- [ ] **Step 4: 後始末を確認する**

`fifty_four_lsp.exe` や `uvicorn`、`docker run` で立てたコンテナが残っていないこと、リポジトリ直下に `characters.md` 以外の生成物が増えていないことを確認する。
