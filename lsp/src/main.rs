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
mod chat_context;
mod character;
mod character_ast;
mod character_updater;
mod code_action;
mod cursor_context;
mod flight_recorder;
mod frontmatter;
mod highlight;
mod llm;
mod logging;
mod progress;
#[cfg(debug_assertions)]
mod session_log;
mod text;
mod tools;
mod types;
#[cfg(debug_assertions)]
mod writing_agent;

use crate::backend::Backend;
// `error` はACP経路(debugビルド限定)のエラーハンドリングでのみ使う。
#[cfg_attr(not(debug_assertions), allow(unused_imports))]
use log::{error, info};

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
    } else {
        load_dev_env();
    }

    async_main(acp)
}

/// Tokio のランタイム上で動作し、標準入出力を通じてクライアントと通信します。
/// 既定では LSP サーバとして動作し、`acp=true` なら ACP エージェント
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
