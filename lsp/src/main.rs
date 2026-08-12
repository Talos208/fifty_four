// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
/// ACP エージェントは debug ビルド限定。
///
/// LLM アクセスに作者自身の `claude` CLI のログイン(= サブスクリプション枠)を
/// そのまま使うため、配布物に載せて第三者へ提供することは Anthropic の規約上できない。
/// 注意書きではなくバイナリの性質として落としておく。
#[cfg(debug_assertions)]
mod acp;
#[cfg(debug_assertions)]
mod acp_config;
mod assets;
mod backend;
mod character;
mod character_ast;
mod character_updater;
mod chat_context;
mod code_action;
mod cursor_context;
mod flight_recorder;
mod frontmatter;
mod highlight;
mod llm;
mod logging;
mod plot;
mod progress;
mod references;
#[cfg(debug_assertions)]
mod session_log;
mod text;
mod tools;
mod types;
#[cfg(debug_assertions)]
mod writing_agent;

use crate::{backend::Backend, logging::Logger};
// `error` はACP経路(debugビルド限定)のエラーハンドリングでのみ使う。
// `tracing-subscriber` の `tracing-log` 連携により、`log::` マクロは(otel 有効時)
// 自動的に tracing イベントへ変換されて OTel へ流れる。既存コード全体を書き換えず
// 計装対象にするため、`tracing::info!` ではなくこちらに統一する。
#[cfg_attr(not(debug_assertions), allow(unused_imports))]
use log::{error, info};

/// `RUST_LOG` は process-wide なので、それを読み書きするテストは同時に走ると競合する。
/// `main.rs`・`logging.rs` 双方のテストがこれを共有し、1本の Mutex で直列化する。
#[cfg(test)]
pub(crate) static RUST_LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// ローカル開発用に、ソースリポジトリのルートにある `.env` を読み込む(debug ビルドのみ)。
///
/// 無い場合(ビルドマシン外へ転送したデバッグビルド等)は OS の環境変数をそのまま使う。
#[cfg(debug_assertions)]
fn load_dev_env() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent");
    if let Err(err) = dotenvx_rs::dotenvx::from_path(repo_root.join(".env")) {
        eprintln!(
            "No .env loaded ({}); using process environment variables as-is",
            err
        );
    }
}

/// `claude` CLI にサブスクリプション枠を使わせるため、Anthropic の API 資格情報を
/// プロセス環境から取り除く。
///
/// SDK は子プロセスへ親の環境を丸ごと渡すので(`env::vars()` → `Command::envs`)、
/// `ClaudeAgentOptions::env` では打ち消せない。認証解決は
/// `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` → ログイン済みプロファイルの順で、
/// キーが在る限り先に勝つため、実際に消すしかない。
///
/// # Safety
/// `remove_var` は他スレッドが環境を読んでいると UB。tokio ランタイムを起こす前の
/// シングルスレッドな時点でのみ呼ぶこと。
#[cfg(debug_assertions)]
fn scrub_anthropic_credentials() {
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if std::env::var_os(key).is_some() {
            // ログ初期化前なので eprintln!。黙って消すと
            // 「なぜ自分のキーが効かないのか」を追えない。
            eprintln!(
                "--acp: {} を無視します(claude CLI のサブスクリプション枠で動かすため)",
                key
            );
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// `--acp` 時、`RUST_LOG` がこのバイナリ向けに指定されていなければ ACP 関連モジュール
/// だけ既定で debug にする。
///
/// Zed は `agent_servers` 経由でこのバイナリを直接起動するため、ターミナルから
/// `RUST_LOG=fifty_four_lsp=debug` を渡す手段が事実上無い(`docs/acp-agent.md` の
/// 動作確認手順はターミナルから直接起動する前提で書かれている)。env_logger の
/// 既定(`error`のみ)だと `session/new` → `session/prompt` の流れや、実際に失敗する
/// 手前の文脈が何も残らない。そこで ACP 関連モジュールに限定して debug を既定化する。
///
/// `RUST_LOG` が既に設定されていても、値にこのバイナリの crate 名
/// (`fifty_four_lsp`)が含まれていなければ上書きする。Zed 自身が `RUST_LOG=lsp=trace`
/// のような自分用の値を設定した状態で起動され、それが素通しで子プロセスへ継承される
/// ケースがあるため(`lsp` というターゲットはこの crate のどのモジュールパスにも
/// マッチせず、かつベアのデフォルトレベルも無いディレクティブなので、素通しすると
/// ACP のログ・トレースが一切出なくなる)。ユーザーが `agent_servers` の `env` 経由で
/// `fifty_four_lsp=...` を明示していれば、それは尊重して上書きしない。
///
/// # Safety
/// `set_var` は他スレッドが環境を読んでいると UB。tokio ランタイムを起こす前の
/// シングルスレッドな時点でのみ呼ぶこと(`scrub_anthropic_credentials` と同じ制約)。
#[cfg(debug_assertions)]
fn default_acp_log_level() {
    if let Ok(existing) = std::env::var("RUST_LOG")
        && existing.contains("fifty_four_lsp")
    {
        // このバイナリ向けの指定が既に含まれている。ユーザー/呼び出し元の
        // 明示的な意図として尊重し、上書きしない。
        return;
    }
    unsafe {
        std::env::set_var(
            "RUST_LOG",
            "warn,fifty_four_lsp::acp=debug,fifty_four_lsp::acp_config=debug,\
             fifty_four_lsp::writing_agent=debug,fifty_four_lsp::session_log=debug",
        );
    }
}

/// プログラムのエントリポイント。
///
/// `--acp` の有無に応じた環境変数の準備を、Tokio ランタイム(マルチスレッド)を
/// 起動する**前**に済ませるため、素の `fn` として `async_main` から分離している
/// (`std::env::remove_var` は他スレッドが環境を読んでいると UB であり、
/// ワーカースレッドが立った後では安全に呼べない)。
fn main() {
    let acp = std::env::args().skip(1).any(|a| a == "--acp");

    // ここはまだシングルスレッド。環境変数の操作は他スレッドが立つ前に済ませる。
    #[cfg(not(debug_assertions))]
    if acp {
        eprintln!(
            "fifty_four_lsp: --acp は debug ビルド限定です \
             (claude CLI のサブスクリプション枠を使うため、配布物には含めていません)"
        );
        std::process::exit(1);
    }

    #[cfg(debug_assertions)]
    if acp {
        // ACP 経路は provider の API キーを一切必要としない(LLM は claude CLI 経由)。
        // .env を読む理由が無いので読まない。
        scrub_anthropic_credentials();
        default_acp_log_level();
    } else {
        load_dev_env();
    }

    async_main(acp)
}

/// `--acp` 時、panic の内容を標準出力のpanicメッセージに加えてファイルへも書き残す。
///
/// ACP経路には従来panic hookが無く、クラッシュしても既定のpanicメッセージが
/// stderrへ一瞬流れるだけだった(Zedがそのstderrを拾わなければ完全に消える)。
/// 既定の出力はそのまま残しつつ、`<実行ファイルの隣>/logs/acp_panic.log` へ
/// 追記する(パス解決は `flight_recorder::FlightRecorder::open_default` と同じ方針:
/// コピー先のフォルダでもそのまま動くよう `current_exe` を基準にする)。
/// バックトレースは `RUST_BACKTRACE` の設定に関わらず常に採取する
/// (panicは滅多に起きないので、そのときくらいは詳細を惜しまない)。
#[cfg(debug_assertions)]
fn install_acp_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backtrace = std::backtrace::Backtrace::force_capture();
        let line = format!("[{now}] {info}\nbacktrace:\n{backtrace}\n\n");
        append_to_acp_panic_log(&line);
    }));
}

/// `logs/acp_panic.log` へ1行(実際には複数行の塊)追記する。
///
/// フック本体から書き込みロジックだけを切り出したもの。`std::panic::PanicHookInfo` は
/// テストコードから組み立てられないため、フォーマット済み文字列を受け取る形にして
/// パス解決・追記処理だけを単体テスト可能にしてある。
#[cfg(debug_assertions)]
fn append_to_acp_panic_log(line: &str) {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    else {
        return;
    };
    let log_dir = exe_dir.join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("acp_panic.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Tokio のランタイム上で動作し、標準入出力を通じてクライアントと通信します。
/// 既定では LSP サーバとして動作し、`acp=true` なら ACP エージェント
/// (Zed の Agent Panel から `agent_servers` 経由で起動される)として動作します。
/// どちらも stdio を JSON-RPC のチャネルとして使うため、ログは stderr へ出す。
#[tokio::main]
async fn async_main(acp: bool) {
    // ロガー/トレーサの設置はここに一本化する。以前は ACP 分岐の手前で
    // env_logger を無条件初期化していたため、Logger::new() 内の
    // tracing_subscriber 初期化(内部で LogTracer::init() を呼ぶ)がグローバル
    // ロガーの二重設定で失敗し、OTel パイプラインが一度も設置されていなかった。
    // ACP 経路も計装対象にするため、分岐より前に置く。
    let _log = Logger::new(acp);

    if acp {
        #[cfg(debug_assertions)]
        {
            install_acp_panic_hook();
            if let Err(e) = acp::run().await {
                // 設定不備などはここで落ちる。Zed のログに理由が残るよう stderr にも出す。
                error!("{}", e);
                eprintln!("fifty_four_lsp --acp: {}", e);
                // std::process::exit はデストラクタを実行しない。ここで明示的に
                // Logger を drop してバッチ済みのログ/スパンを送出させてから終了する
                // (でないと、一番見たいはずの落ちた瞬間の記録が丸ごと消える)。
                drop(_log);
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(debug_assertions))]
        unreachable!("--acp は main で弾いている");
    }

    // 標準入力／出力を LSP の通信チャネルとして利用
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    info!("initialize lsp service");
    let (service, socket) = tower_lsp_server::LspService::build(Backend::new).finish();

    // サーバを起動してクライアントとのメッセージループを開始する
    info!("start server");

    tower_lsp_server::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    use crate::RUST_LOG_TEST_LOCK as ENV_LOCK;

    #[test]
    fn test_default_acp_log_level_sets_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("RUST_LOG") };

        default_acp_log_level();

        let value = std::env::var("RUST_LOG").unwrap();
        assert!(value.contains("fifty_four_lsp::acp=debug"));
        assert!(value.contains("fifty_four_lsp::acp_config=debug"));
        assert!(value.contains("fifty_four_lsp::writing_agent=debug"));
        assert!(value.contains("fifty_four_lsp::session_log=debug"));

        unsafe { std::env::remove_var("RUST_LOG") };
    }

    #[test]
    fn test_default_acp_log_level_does_not_override_existing_when_it_names_the_crate() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("RUST_LOG", "warn,fifty_four_lsp=trace") };

        default_acp_log_level();

        assert_eq!(std::env::var("RUST_LOG").unwrap(), "warn,fifty_four_lsp=trace");

        unsafe { std::env::remove_var("RUST_LOG") };
    }

    /// Zed 自身の `RUST_LOG=lsp=trace` のような、このバイナリと無関係な値が
    /// 継承されているケース。`lsp` はこの crate (`fifty_four_lsp`) のどのモジュール
    /// パスにもマッチしないため、素通しすると ACP のログが一切出なくなる
    /// (実際にこの値が原因で発生した不具合の再現テスト)。
    #[test]
    fn test_default_acp_log_level_overrides_unrelated_inherited_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("RUST_LOG", "lsp=trace") };

        default_acp_log_level();

        let value = std::env::var("RUST_LOG").unwrap();
        assert!(value.contains("fifty_four_lsp::acp=debug"));

        unsafe { std::env::remove_var("RUST_LOG") };
    }

    /// panic hook 本体(`std::panic::set_hook`)はプロセス全体に効いてしまい、
    /// 他のテストの `#[should_panic]` 等に副作用が及ぶため、書き込みロジックだけを
    /// 切り出した [`append_to_acp_panic_log`] を直接呼んで検証する。
    #[test]
    fn test_append_to_acp_panic_log_writes_expected_file() {
        let log_path = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("logs")
            .join("acp_panic.log");
        let before = std::fs::read_to_string(&log_path).unwrap_or_default();

        let marker = format!(
            "TEST-MARKER-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        append_to_acp_panic_log(&format!("{marker}\n"));

        let after = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            after.starts_with(&before),
            "既存の内容を上書きしていないこと"
        );
        assert!(after.contains(&marker), "追記した内容が読めること");
    }
}
