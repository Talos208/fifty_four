// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
}

/// `LanguageServer` トレイトの実装。
///
/// ここでは最小限のメソッドのみ実装しており、将来的にホバーや補完などを追加できます。
#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// LSP クライアントからの `initialize` リクエストに応答します。
    ///
    /// 返却する `InitializeResult` でサーバの機能（capabilities）をクライアントに伝えます。
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            // 現在はデフォルト（機能なし）を返す
            capabilities: ServerCapabilities::default(),
            server_info: None,
        })
    }

    /// `initialized` はクライアントが初期化完了を通知した際に呼ばれます。
    ///
    /// ここではデバッグ用にログメッセージをクライアントへ送信しています。
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "LSP server initialized")
            .await;
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// プログラムのエントリポイント。
///
/// Tokio のランタイム上で動作し、標準入出力を通じて LSP クライアントと通信します。
#[tokio::main]
async fn main() {
    // 標準入力／出力を LSP の通信チャネルとして利用
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    let (service, socket) = tower_lsp::LspService::build(|client| Backend { client }).finish();

    // サーバを起動してクライアントとのメッセージループを開始する
    tower_lsp::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;
}
