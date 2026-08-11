//! `main.rs` から切り出したモジュール。completion 実行中の LSP WorkDoneProgress 表示を扱う。

#[allow(unused_imports)]
use log::debug;
use tower_lsp_server::Client;
use tower_lsp_server::lsp_types::*;
use tracing::instrument;

/// completion 実行中に LSP WorkDoneProgress を表示するためのガード。
/// LLM 応答待ちの間、Zed の activity indicator(左下ステータスバー)に「処理中」を出す。
/// Zed のタイムアウト/再入力による $/cancelRequest で completion の future が
/// drop された場合でも、Drop 実装が必ず End 通知を送り、表示の残留を防ぐ。
pub(crate) struct CompletionProgress {
    client: Client,
    token: ProgressToken,
    finished: bool,
}

impl CompletionProgress {
    /// 進捗表示を開始する。
    /// - クライアントがリクエストに workDoneToken を付けてきた場合はそれを使う(create 不要)
    /// - 無ければ window/workDoneProgress/create でサーバ発トークンを登録する
    ///   (クライアントが window.workDoneProgress 非対応、または create 拒否なら None)
    #[instrument(skip(client, title, message))]
    pub(crate) async fn begin(
        client: &Client,
        supported: bool,
        client_token: Option<ProgressToken>,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Option<Self> {
        let token = match client_token {
            // リクエスト付属トークン(LSP 仕様上 create は不要)
            Some(t) => t,
            None => {
                if !supported {
                    return None;
                }
                static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let token = ProgressToken::String(format!(
                    "54/completion/{}",
                    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                if let Err(e) = client
                    .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                        token: token.clone(),
                    })
                    .await
                {
                    debug!("WorkDoneProgressCreate rejected: {:?}", e);
                    return None;
                }
                token
            }
        };

        // OngoingProgress は型パラメータ付きでフィールド保持が煩雑なため使わず、
        // End 通知は send_end() で直接送る(ここでは begin 通知の送信だけが目的)。
        let _ = client
            .progress(token.clone(), title)
            .with_message(message)
            .begin()
            .await;

        Some(Self {
            client: client.clone(),
            token,
            finished: false,
        })
    }

    /// End 通知を送って進捗表示を消す(正常終了経路)。
    #[instrument(skip(self))]
    pub(crate) async fn finish(mut self) {
        self.finished = true;
        Self::send_end(&self.client, self.token.clone()).await;
    }

    #[instrument(skip(client))]
    async fn send_end(client: &Client, token: ProgressToken) {
        client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: None,
                })),
            })
            .await;
    }
}

impl Drop for CompletionProgress {
    #[instrument(skip(self))]
    fn drop(&mut self) {
        if !self.finished {
            // キャンセル等で future ごと drop された場合。Drop 内では await できないため
            // 別タスクで End を送る(送らないと Zed 側の進捗表示が残留し続ける)。
            debug!("Finish progress");
            let client = self.client.clone();
            let token = self.token.clone();
            tokio::spawn(async move { Self::send_end(&client, token).await });
        }
    }
}
