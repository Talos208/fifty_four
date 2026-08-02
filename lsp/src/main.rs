// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
/// ACP エージェントは debug ビルド限定。
///
/// LLM アクセスに作者自身の `claude` CLI のログイン(= サブスクリプション枠)を
/// そのまま使うため、配布物に載せて第三者へ提供することは Anthropic の規約上できない。
/// 注意書きで済ませず、release バイナリからは機能ごと落としておく。
#[cfg(debug_assertions)]
mod acp;
mod assets;
mod backend;
/// `chat_context` は cfg で落とさない。書く側([`acp`])は debug 限定だが、
/// 読む側([`backend`])は release でも動く。
mod chat_context;
mod character;
mod character_updater;
mod code_action;
mod cursor_context;
mod flight_recorder;
mod frontmatter;
mod highlight;
mod llm;
mod logging;
mod progress;
mod text;
mod tools;
mod types;
#[cfg(debug_assertions)]
mod writing_agent;

use crate::backend::Backend;
/// `error!` を使うのは ACP の起動失敗時だけなので、release では取り込まない。
#[cfg(debug_assertions)]
use log::error;
use log::info;

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
/// プロセス環境から取り除く(debug ビルドの `--acp` 起動時のみ)。
///
/// `anthropic-agent-sdk` は子プロセスへ親の環境を丸ごと渡すので
/// (`env::vars()` を集めて `Command::envs`)、`ClaudeAgentOptions::env` では打ち消せない。
/// insert しかできず、空文字を入れても「空のキーで認証」になるだけである。
/// Anthropic の認証解決は `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` →
/// ログイン済みプロファイルの順で、**キーが在る限り先に勝つ**ため実際に消すしかない。
///
/// 消すのは認証に使われるこの 2 つだけ。`ANTHROPIC_BASE_URL` 等には触らない。
///
/// # Safety
///
/// [`std::env::remove_var`] は他スレッドが環境変数を読んでいると未定義動作になる。
/// Tokio ランタイムを起こす前、まだシングルスレッドの時点でのみ呼ぶこと。
#[cfg(debug_assertions)]
fn scrub_anthropic_credentials() {
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if std::env::var_os(key).is_some() {
            // ロガー初期化前なので eprintln!。黙って消すと
            // 「なぜ自分のキーが効かないのか」を追えなくなる。
            eprintln!(
                "--acp: {} を無視します(claude CLI のサブスクリプション枠で動かすため)",
                key
            );
            // SAFETY: 呼び出し元(`main`)は Tokio ランタイムより前のシングルスレッド。
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// プログラムのエントリポイント。
///
/// 非同期ランタイムより**前**に環境変数を整えるため、ここは素の `fn` にしてある
/// ([`std::env::remove_var`] は他スレッドが立つ前に呼ぶ必要があるため)。
/// 実際の処理は [`async_main`] へ。
fn main() {
    let acp = std::env::args().skip(1).any(|a| a == "--acp");

    // --- ここはまだシングルスレッド。環境変数の操作はすべてこの区間で済ませる ---

    // release ビルドに ACP エージェントは入っていない(モジュールごと cfg で落としてある)。
    #[cfg(not(debug_assertions))]
    if acp {
        eprintln!(
            "fifty_four_lsp: --acp は debug ビルド限定です\
             (claude CLI のサブスクリプション枠を使うため、配布物には含めていません)"
        );
        std::process::exit(1);
    }

    #[cfg(debug_assertions)]
    if acp {
        // ACP 経路は provider の API キーを一切必要としない(LLM アクセスは claude CLI 経由)。
        // `.env` を読む理由が無いどころか、読むと ANTHROPIC_API_KEY が入って
        // サブスクリプション枠ではなく API クレジット課金になってしまう。
        scrub_anthropic_credentials();
    } else {
        load_dev_env();
    }

    async_main(acp)
}

/// 非同期の本体。
///
/// Tokio のランタイム上で動作し、標準入出力を通じてクライアントと通信します。
/// 既定では LSP サーバとして動作し、`--acp` を付けると ACP エージェント
/// (Zed の Agent Panel から `agent_servers` 経由で起動される)として動作します。
/// どちらも stdio を JSON-RPC のチャネルとして使うため、ログは stderr へ出す。
#[tokio::main]
async fn async_main(acp: bool) {
    env_logger::Builder::from_default_env()
        .format_target(false)
        .format_module_path(false)
        .format_source_path(true)
        .target(env_logger::Target::Stderr)
        .init();

    if acp {
        #[cfg(debug_assertions)]
        {
            if let Err(e) = acp::run().await {
                // 設定不備などはここで落ちる。Zed のログに理由が残るよう stderr にも出す。
                error!("{}", e);
                eprintln!("fifty_four_lsp --acp: {}", e);
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
