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
mod outline;
mod plot;
mod plot_sync;
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
use tracing::instrument;

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
/// プロセス環境から取り除く。**`load_dev_env()` の直後に呼ぶこと**(順序を逆にすると
/// `.env` のキーが残ったまま `claude` CLI が起動し、API キー課金で動いてしまう)。
/// 背景(SDK が環境を打ち消せない理由・認証解決順)は `docs/acp-agent.md` の「認証」参照。
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
                "--acp: {} を無視します(シェル/Zed からの継承、または .env 由来。\
                 claude CLI のサブスクリプション枠で動かすため)",
                key
            );
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// `--acp` 時、`RUST_LOG` にこの crate 名への言及が無ければ ACP 関連モジュールだけ
/// 既定で debug にする。背景(Zed からは `RUST_LOG` を渡す手段が事実上無いこと、
/// 「言及が無ければ」という判定にしている理由)は `docs/observability.md` の
/// 「`RUST_LOG` のスコープ」参照。
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
        // 会話本体(claude CLI)はサブスク枠のままだが、要約(chat digest)は
        // `llm.rs` の provider(Gemini 等)を使うため、そちらの API キーを
        // `.env` から読む必要がある(`crate::acp::update_digest` 参照)。
        // 読んだ直後に Anthropic の資格情報だけ消すので、`claude` CLI が
        // 誤って API キー課金へ落ちることはない
        // (**順序が重要**: 消す前に読むと Anthropic のキーも一瞬入るが、
        // scrub が必ずそれを取り除いてから claude CLI を起動する)。
        load_dev_env();
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
#[instrument]
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

        assert_eq!(
            std::env::var("RUST_LOG").unwrap(),
            "warn,fifty_four_lsp=trace"
        );

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

    /// `--acp` 時、`.env` を読んだ**あと**に Anthropic の資格情報を消すという順序を
    /// 固定する回帰テスト。逆順(消してから読む)にすると `.env` の
    /// `ANTHROPIC_API_KEY` が生き残り、`claude` CLI がサブスク枠ではなく
    /// API キー課金で動いてしまう(要約用に `load_dev_env()` を足したときに
    /// 一番壊しやすい箇所なので、`main()` の呼び出し順そのものではなく、
    /// 「読み込み後に scrub すれば必ず消える」という性質をここで固定する)。
    #[test]
    fn test_scrub_after_load_dev_env_removes_anthropic_keys_regardless_of_source() {
        let _guard = ENV_LOCK.lock().unwrap();
        // `.env` を読んだ直後の状態を模して、Anthropic のキーが環境に入っている
        // ケースを再現する(実際には dotenvx が .env から復号して入れる)。
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-dummy-from-dotenv");
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "dummy-token-from-dotenv");
        }

        scrub_anthropic_credentials();

        assert!(std::env::var_os("ANTHROPIC_API_KEY").is_none());
        assert!(std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_none());
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
