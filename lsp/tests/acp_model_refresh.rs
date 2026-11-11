//! 動いている ACP エージェントの下で `claude` CLI が更新されたとき、モデルの選択肢が新しくなるか。
//!
//! 実物の `claude` を2つ使う(偽物では「本物の CLI が返す一覧が出るか」を確かめられないため)。
//! 実際の自動更新と同じく、動いている `claude` 実行ファイルをリネームして新しいものを置く。
//!
//! 1. 新 CLI で起動したエージェントの一覧を正解(C)とする
//! 2. 旧 CLI でエージェントを起動し、会話1を開く(一覧 A)。A にあって C に無いモデルがあれば、
//!    会話1でそれを選んでおく(CLI の更新で選んでいたモデルが消える場合)
//! 3. CLI を新に差し替え、同じエージェントで会話2を開く
//! 4. 会話2には C が、会話1には C と選んでいたモデルが、`config_option_update` で届くこと
//!
//! プロンプトは送らないので課金は無いが、ログイン済みの `claude` と、モデル一覧の違う
//! 2つの版が要るので、通常は走らせない:
//!
//! ```text
//! FF_TEST_OLD_CLAUDE=<旧 claude の実行ファイル> cargo test --test acp_model_refresh -- --ignored --nocapture
//! ```
//!
//! 新 CLI は `FF_TEST_NEW_CLAUDE`、無ければ PATH 上の `claude`。
//! `--acp` は debug ビルド限定なので、release ではこのテスト自体が無い。
#![cfg(debug_assertions)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// 一覧の更新通知を待つ上限。問い合わせ自体は1秒程度。
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// 要求の応答を待つ上限。`session/new` は普段1〜2秒だが、コピーしたばかりの `claude` を
/// 初めて起動するときなどに 16〜29 秒かかったことがある(原因は未確認)。
/// 更新通知の待ちとは分け、機能と関係ない遅さで落ちないようにする。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

const CLI_NAME: &str = if cfg!(windows) {
    "claude.exe"
} else {
    "claude"
};

/// `(value, name)` の並び。
type ModelList = Vec<(String, String)>;

/// stdio で喋る `fifty_four_lsp --acp`。drop で必ずプロセスを落とす
/// (アサーションで panic しても、エージェントと配下の `claude` を残さないため)。
struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    /// 応答を待つ間に届いた通知。
    notes: Vec<Value>,
}

impl Agent {
    fn start(bin_dir: &Path, ws: &Path) -> Self {
        let path = std::env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_fifty_four_lsp"))
            .arg("--acp")
            .current_dir(ws)
            .env("PATH", path)
            .env("RUST_LOG", "fifty_four_lsp=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 失敗したときに追えるよう、エージェントのログは `--nocapture` でそのまま見せる。
            .stderr(Stdio::inherit())
            .spawn()
            .expect("エージェントを起動できる");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut agent = Self {
            child,
            stdin,
            rx,
            next_id: 1,
            notes: Vec::new(),
        };
        agent.call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        agent
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let v = self
                .rx
                .recv_timeout(REQUEST_TIMEOUT)
                .unwrap_or_else(|e| panic!("{method} の応答が来ない: {e}"));
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            self.notes.push(v);
        }
    }

    /// 会話を開き、`(セッションID, 応答に載っていたモデル一覧)` を返す。
    fn new_session(&mut self, ws: &Path) -> (String, ModelList) {
        let res = self.call("session/new", json!({"cwd": ws, "mcpServers": []}));
        let result = res
            .get("result")
            .unwrap_or_else(|| panic!("session/new が失敗した: {res}"));
        let id = result["sessionId"].as_str().unwrap().to_string();
        (id, model_list(&result["configOptions"]))
    }

    /// 会話のモデルを選ぶ(`session/set_config_option`)。
    fn select_model(&mut self, session_id: &str, value: &str) {
        let res = self.call(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "model", "value": value}),
        );
        assert!(res.get("result").is_some(), "モデルを選べない: {res}");
    }

    /// `session_id` 宛ての `config_option_update` を待ち、`(モデル一覧, 選ばれているモデル)` を返す。
    fn wait_config_update(&mut self, session_id: &str) -> Option<(ModelList, String)> {
        let is_update = |v: &Value| {
            v["method"] == "session/update"
                && v["params"]["sessionId"] == session_id
                && v["params"]["update"]["sessionUpdate"] == "config_option_update"
        };
        let read = |v: &Value| {
            let opts = &v["params"]["update"]["configOptions"];
            (model_list(opts), current_model(opts))
        };
        if let Some(v) = self.notes.iter().rev().find(|v| is_update(v)) {
            return Some(read(v));
        }
        let deadline = Instant::now() + UPDATE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) if is_update(&v) => return Some(read(&v)),
                Ok(v) => self.notes.push(v),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn model_option(config_options: &Value) -> &Value {
    config_options
        .as_array()
        .and_then(|opts| opts.iter().find(|o| o["id"] == "model"))
        .unwrap_or_else(|| panic!("model の設定項目が無い: {config_options}"))
}

fn current_model(config_options: &Value) -> String {
    model_option(config_options)["currentValue"]
        .as_str()
        .unwrap()
        .to_string()
}

fn model_list(config_options: &Value) -> ModelList {
    model_option(config_options)["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["value"].as_str().unwrap().to_string(),
                o["name"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// 試験用の作業ディレクトリ(`claude` 実行ファイルのコピーが 2 つ入り、数百 MB になる)。
///
/// drop で消す。エージェントを kill しても、その子の `claude` は少し遅れて終わるので、
/// 使用中で消せない間は待って消し直す(`TempDir` の drop は失敗しても黙るので、それに任せない)。
/// エージェントより先に作り、あとで drop されるようにする。
struct Workdir(PathBuf);

impl Workdir {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap().keep())
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) if Instant::now() >= deadline => {
                    eprintln!(
                        "作業ディレクトリを消せませんでした({}): {e}",
                        self.0.display()
                    );
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }
}

fn cli_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

#[test]
#[ignore = "ログイン済みの実物の claude を2版使う。FF_TEST_OLD_CLAUDE を指定して手で走らせる"]
fn model_list_follows_cli_update_while_agent_is_running() {
    let old_cli = cli_path("FF_TEST_OLD_CLAUDE")
        .expect("FF_TEST_OLD_CLAUDE に、モデル一覧が今と違う旧版の claude を指定する");
    let new_cli = cli_path("FF_TEST_NEW_CLAUDE")
        .or_else(|| which::which("claude").ok())
        .expect("FF_TEST_NEW_CLAUDE か PATH 上に新版の claude が要る");

    let work = Workdir::new();
    let bin_dir = work.0.join("bin");
    let ws = work.0.join("ws");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let live_cli = bin_dir.join(CLI_NAME);
    std::fs::copy(&old_cli, &live_cli).unwrap();

    // 1. 正解: 新 CLI で起動したエージェントの一覧(新 CLI のあるディレクトリを PATH の先頭に置く)
    let expected = {
        let mut fresh = Agent::start(new_cli.parent().unwrap(), &ws);
        fresh.new_session(&ws).1
    };

    // 2. 旧 CLI で起動して会話1
    let mut agent = Agent::start(&bin_dir, &ws);
    let started = Instant::now();
    let (s1, a) = agent.new_session(&ws);
    eprintln!("会話1の session/new: {:?}", started.elapsed());
    assert_ne!(
        a, expected,
        "旧と新の claude が同じモデル一覧を返すので、この試験では判別できない。\
         一覧の違う旧版を FF_TEST_OLD_CLAUDE に指定する"
    );
    // CLI の更新で消える選択肢を、会話1で選んでおく(無ければ既定のまま)
    let vanishing = a
        .iter()
        .find(|(v, _)| !expected.iter().any(|(e, _)| e == v))
        .cloned();
    if let Some((value, _)) = &vanishing {
        agent.select_model(&s1, value);
    }
    eprintln!("会話1で選んだ、更新で消えるモデル: {vanishing:?}");

    // 3. 自動更新と同じ手順で差し替え(動いている実行ファイルはリネームできる)、会話2
    std::fs::rename(&live_cli, bin_dir.join(format!("old-{CLI_NAME}"))).unwrap();
    std::fs::copy(&new_cli, &live_cli).unwrap();
    let started = Instant::now();
    let (s2, _) = agent.new_session(&ws);
    eprintln!("会話2の session/new: {:?}", started.elapsed());

    // 4. 会話2には正解の一覧が届く
    let got2 = agent.wait_config_update(&s2).map(|(list, _)| list);
    assert_eq!(
        got2.as_ref(),
        Some(&expected),
        "CLI を差し替えたあとに開いた会話に、新しい一覧が届かない"
    );
    // 差し替え前から開いていた会話1にも届く。選んでいたモデルは消えずに残り、選ばれたまま
    let (want1, want_current) = match &vanishing {
        Some(kept) => (
            expected.iter().cloned().chain([kept.clone()]).collect(),
            kept.0.clone(),
        ),
        None => (expected.clone(), "default".to_string()),
    };
    let got1 = agent.wait_config_update(&s1);
    assert_eq!(
        got1.as_ref(),
        Some(&(want1, want_current)),
        "差し替え前から開いていた会話に、新しい一覧が届かない"
    );
}
