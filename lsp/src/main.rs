// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
mod acp;
mod assets;
mod backend;
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

use crate::backend::Backend;
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

/// プログラムのエントリポイント。
///
/// Tokio のランタイム上で動作し、標準入出力を通じてクライアントと通信します。
/// 既定では LSP サーバとして動作し、`--acp` を付けると ACP エージェント
/// (Zed の Agent Panel から `agent_servers` 経由で起動される)として動作します。
/// どちらも stdio を JSON-RPC のチャネルとして使うため、ログは stderr へ出す。
#[tokio::main]
async fn main() {
    // 環境変数の初期化
    #[cfg(debug_assertions)]
    load_dev_env();

    env_logger::Builder::from_default_env()
        .format_target(false)
        .format_module_path(false)
        .format_source_path(true)
        .target(env_logger::Target::Stderr)
        .init();

    if std::env::args().skip(1).any(|a| a == "--acp") {
        if let Err(e) = acp::run().await {
            // 設定不備などはここで落ちる。Zed のログに理由が残るよう stderr にも出す。
            error!("{}", e);
            eprintln!("fifty_four_lsp --acp: {}", e);
            std::process::exit(1);
        }
        return;
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
