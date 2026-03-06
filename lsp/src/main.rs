// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use tower_lsp::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, async_trait};
use tracing::{debug, info, instrument, span, warn};
mod highlight;
use crate::highlight::tokenize_conversation;
mod logging;
use crate::logging::Logger;
use dashmap::DashMap;
mod llm;
use crate::llm::{Content, GenericLlmClient, LlmClient};
use rusqlite;
use std::io::Read;
use std::path::PathBuf;
mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}
use indoc::{formatdoc, indoc};
use regex::Regex;
use std::ops::{Deref, DerefMut};

/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
    text: DashMap<String, Vec<String>>,
    llm: Option<tokio::sync::Mutex<Box<dyn LlmClient>>>,
    data_path: PathBuf,
    conn: std::sync::Mutex<rusqlite::Connection>,
}

/// `LanguageServer` トレイトの実装。
///
/// ここでは最小限のメソッドのみ実装しており、将来的にホバーや補完などを追加できます。
#[async_trait]
impl LanguageServer for Backend {
    /// LSP クライアントからの `initialize` リクエストに応答します。
    ///
    /// 返却する `InitializeResult` でサーバの機能（capabilities）をクライアントに伝えます。
    #[instrument(ret, err)]
    async fn initialize(&self, _param: InitializeParams) -> Result<InitializeResult> {
        // サーバの機能（capabilities）を構成します。
        // ここでは最小限として semanticTokens の提供（空実装）を宣言します。

        self.client
            .log_message(
                MessageType::INFO,
                format!("Workspace: {:?}", _param.workspace_folders),
            )
            .await;

        if let Some(info) = _param.client_info {
            self.client
                .log_message(MessageType::INFO, format!("Client_info: {:?}", info))
                .await;
        }

        if let Some(opt) = _param.initialization_options {
            self.client
                .log_message(MessageType::INFO, format!("Options: {:?}", opt))
                .await;
        }

        let capabilities = ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF8),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
            selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
            // document_highlight_provider: Some(OneOf::Left(true)),
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                    SemanticTokensRegistrationOptions {
                        text_document_registration_options: TextDocumentRegistrationOptions {
                            document_selector: Some(vec![DocumentFilter {
                                language: Some("fifty_four".to_string()),
                                scheme: Some("file".to_string()),
                                pattern: None,
                            }]),
                        },
                        semantic_tokens_options: SemanticTokensOptions {
                            // 代表的なトークン種類を列挙しておく（クライアントが期待するため）
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::KEYWORD,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::VARIABLE,
                                ],
                                token_modifiers: vec![],
                            },
                            // フル（ドキュメント全体）の要求に応答することを示す
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            // 範囲クエリのサポートは無効（今回は実装しない）
                            range: Some(false),
                            // work_done_progress のオプション（未使用）
                            work_done_progress_options: WorkDoneProgressOptions {
                                work_done_progress: None,
                            },
                        },
                        static_registration_options: StaticRegistrationOptions::default(),
                    },
                ),
            ),
            completion_provider: Some(CompletionOptions {
                resolve_provider: None,
                trigger_characters: Some(vec!["、".into(), "「".into(), "『".into()]),
                all_commit_characters: Some(vec!["。".into(), "」".into(), "』".into()]),
                work_done_progress_options: WorkDoneProgressOptions {
                    work_done_progress: None,
                },
                completion_item: Some(CompletionOptionsCompletionItem {
                    label_details_support: None,
                }),
            }),
            ..ServerCapabilities::default()
        };

        Ok(InitializeResult {
            capabilities,
            server_info: None,
        })
    }

    /// `initialized` はクライアントが初期化完了を通知した際に呼ばれます。
    ///
    /// ここではデバッグ用にログメッセージをクライアントへ送信しています。
    #[instrument(ret)]
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "LSP server initialized")
            .await;
    }

    #[instrument(ret)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file opened!")
            .await;

        // 行ごとに分割しておく
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        let texts = cr
            .split(params.text_document.text.as_str())
            .map(|s| s.to_string())
            .collect();
        self.update_all(params.text_document.uri.as_str(), 0, texts);
    }

    #[instrument]
    async fn did_change(&self, param: DidChangeTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "did_change")
            .await;

        // 全体が送られて来た時
        if param
            .content_changes
            .iter()
            .all(|c| c.range.is_none() && c.range_length.is_none())
        {
            let cr = Regex::new(r"\r\n|\r|\n").unwrap();
            self.update_all(
                param.text_document.uri.as_str(),
                0,
                param
                    .content_changes
                    .iter()
                    .flat_map(|c| cr.split(c.text.as_str()).map(|s| s.to_string()))
                    .collect(),
            );
            return;
        }

        self.client
            .log_message(MessageType::INFO, format!("param {:?}", param))
            .await;

        self.update_partial(
            param.text_document.uri.as_str(),
            param.content_changes.as_ref(),
        );
    }

    #[instrument(ret)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file closed!")
            .await;

        self.text.remove(params.text_document.uri.as_str());
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    #[instrument(ret, err)]
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// ドキュメント全体に対する semantic tokens の問い合わせに応答します。
    ///
    /// 現状はトークン配列を空で返し、クライアントに対して "サーバは semantic tokens を提供する"
    /// ことを示すための最小実装です。後で実際のトークン列を生成する処理を追加できます。
    #[instrument(ret, err)]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.client
            .log_message(MessageType::LOG, "semantic_token_full")
            .await;

        let mut line_no = 0;
        let mut vec = vec![];
        let s = self
            .text
            .get(params.text_document.uri.as_ref())
            .expect("Failed to get text");
        s.iter().for_each(|s| {
            let tokens = tokenize_conversation(s.as_str());

            let mut data: Vec<SemanticToken> = tokens // .windows(2).map(|v| (v.first(), v.last()))
                .iter()
                .map(|token| {
                    SemanticToken {
                        delta_line: line_no, // TODO: これではダメ。差分にしないと
                        delta_start: token.start,
                        length: token.length,
                        token_type: token.token_type,
                        token_modifiers_bitset: token.modifier,
                    }
                })
                .collect();
            vec.append(&mut data);
            line_no += 1;
        });

        let tokens = SemanticTokens {
            result_id: None,
            data: vec,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    // #[instrument(ret, err)]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.client
            .log_message(MessageType::LOG, "completion")
            .await;

        if let Some(context) = params.context {
            match context.trigger_kind {
                CompletionTriggerKind::INVOKED => {
                    // Handle completion triggered by user input
                }
                CompletionTriggerKind::TRIGGER_CHARACTER => {
                    // Handle completion triggered by a specific character
                }
                CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS => {
                    // Handle completion triggered for incomplete completion
                }
                _ => {
                    // Handle other trigger kinds
                }
            }
        }

        let uri = params.text_document_position.text_document.uri.as_str();
        let text = self.text.get(uri).unwrap();
        let line_no = params.text_document_position.position.line as usize;
        let offset = params.text_document_position.position.character as usize;

        let before = text
            .iter()
            .take(line_no)
            .map(|s| s.as_str())
            .collect::<Vec<&str>>()
            .join("\n");

        self.client
            .log_message(MessageType::LOG, before.as_str())
            .await;

        let line: &str = text[line_no].as_ref();
        let left: String = line.chars().take(offset).collect();

        self.client
            .log_message(MessageType::LOG, left.as_str())
            .await;

        let mut prompt = String::from("");
        if let Ok(mut f) = std::fs::File::open(self.data_path.join("prompt_completion.md")) {
            f.read_to_string(&mut prompt).ok();
        }

        if offset > 0 || !before.is_empty() {
            let mut completion_id = 0u32;
            let raw = self
                .use_llm(
                    async |l| {
                        l.add(Content::Text(prompt));
                        l.add(Content::Text(before));
                        l.add(Content::Text(left));

                        {
                            let db = self.conn.lock().unwrap();
                            let prompt = l.build_content();
                            match db.query_row(
                                indoc!(
                                    "INSERT INTO completions
                                    (document_uri, cursor_line, cursor_character, model_name, prompt)
                                    VALUES (?,?,?,?,?) RETURNING id;"
                                ),
                                rusqlite::params![
                                    uri,
                                    line_no.to_string().as_str(),
                                    offset.to_string().as_str(),
                                    l.get_model(),
                                    prompt.as_str(),
                                ],
                                |row| row.get(0),
                            ) {
                                Ok(r) => {
                                    completion_id = r;
                                }
                                Err(e) => eprintln!("Failed to insert completion: {}", e),
                            };
                        }

                        l.chat().await
                    },
                )
                .await;

            match raw {
                Ok(response) => {
                    let cr = Regex::new(r"\r\n|\r|\n").unwrap();

                    let db = self.conn.lock().unwrap();
                    let items = cr
                        .split(response.as_str())
                        .inspect(|i| {
                            db.execute(
                                indoc!(
                                    "INSERT INTO completion_candidates
                                    (completion_id, rank, candidate)
                                    VALUES (?,?,?);"
                                ),
                                rusqlite::params![completion_id, 0, i],
                            )
                            .unwrap_or_else(|err| {
                                eprintln!("Failed to insert completion_candidate: {}", err);
                                0
                            });
                        })
                        .map(|r| CompletionItem {
                            label: r.to_string(),
                            kind: Some(CompletionItemKind::TEXT),
                            ..Default::default()
                        })
                        .collect();
                    let list = CompletionList {
                        is_incomplete: false,
                        items,
                    };

                    Ok(Some(CompletionResponse::List(list)))
                }
                Err(err) => Err(tower_lsp::jsonrpc::Error::invalid_params(err.to_string())),
            }
        } else {
            Err(tower_lsp::jsonrpc::Error::invalid_params(
                "offset <=0 && before.is_empty()",
            ))
        }
    }

    // #[instrument(ret, err)]
    // async fn document_highlight(
    //     &self,
    //     params: DocumentHighlightParams,
    // ) -> Result<Option<Vec<DocumentHighlight>>> {
    //     self.client
    //         .log_message(MessageType::LOG, "document_highlight")
    //         .await;

    //     let uri = params
    //         .text_document_position_params
    //         .text_document
    //         .uri
    //         .as_str();
    //     let pos = params.text_document_position_params.position;
    //     let (lineno, offset) = (pos.line, pos.character);

    //     let line = {
    //         let kv = self.text.get(uri).unwrap();
    //         let text = kv.value();
    //         text.get(lineno as usize).unwrap_or(&"".to_string()).clone()
    //     };
    //     let result = {
    //         let (_, subline) = line.split_at(offset as usize);
    //         let tokens = tokenize_conversation(subline);
    //         Some(
    //             tokens
    //                 .iter()
    //                 .map(|token| DocumentHighlight {
    //                     range: Range {
    //                         start: Position {
    //                             line: lineno,
    //                             character: offset + token.start,
    //                         },
    //                         end: Position {
    //                             line: lineno,
    //                             character: offset + token.start + token.length,
    //                         },
    //                     },
    //                     kind: Some(DocumentHighlightKind::TEXT),
    //                 })
    //                 .collect(),
    //         )
    //     };

    //     Ok(result)
    // }
}

fn apply_changes(lines: &mut Vec<String>, changes: &[TextDocumentContentChangeEvent]) {
    for change in changes {
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        match change.range {
            None => {
                *lines = cr
                    .split(change.text.as_str())
                    .map(|s| s.to_string())
                    .collect();
            }
            Some(range) => {
                let start_line = range.start.line as usize;
                let start_char = range.start.character as usize;
                let end_line = range.end.line as usize;
                let end_char = range.end.character as usize;

                let prefix = lines
                    .get(start_line)
                    .map(|l| {
                        let ix = start_char.min(l.len());
                        let n = l.chars().take(ix);
                        String::from_iter(n)
                    })
                    .unwrap_or("".to_string());
                let suffix = lines
                    .get(end_line)
                    .map(|l| {
                        let ix = end_char.min(l.len());
                        let n = l.chars().skip(ix);
                        String::from_iter(n)
                    })
                    .unwrap_or("".to_string());

                let mut new_text =
                    String::with_capacity(prefix.len() + change.text.len() + suffix.len());
                new_text.push_str(&prefix);
                new_text.push_str(&change.text);
                new_text.push_str(&suffix);

                let new_lines: Vec<String> =
                    cr.split(new_text.as_str()).map(|s| s.to_string()).collect();

                let end = end_line.min(lines.len() - 1);
                lines.splice(start_line..=end, new_lines);
            }
        }
    }
}

impl Backend {
    async fn use_llm<F>(&self, proc: F) -> core::result::Result<String, Box<dyn core::error::Error>>
    where
        F: for<'b, 'a> AsyncFnOnce(
            &'b mut Box<dyn LlmClient + 'a>,
        )
            -> core::result::Result<String, Box<dyn core::error::Error>>,
    {
        let ref_llm = self.llm.as_ref();
        if let Some(llm) = ref_llm {
            let mut locked = llm.lock().await;
            let llm = locked.deref_mut();
            proc(llm).await
        } else {
            core::result::Result::Err(Box::new(tower_lsp::jsonrpc::Error {
                code: ErrorCode::ServerError(-32002),
                message: std::borrow::Cow::Borrowed("LLM not initialized"),
                data: None,
            }))
        }
    }

    fn update_all(&self, uri: &str, _offset: u32, texts: Vec<String>) {
        self.text.insert(uri.to_string(), texts);
    }

    fn update_partial(&self, uri: &str, changes: &[TextDocumentContentChangeEvent]) {
        let Some(mut lines) = self.text.get_mut(uri) else {
            return;
        };
        apply_changes(&mut lines, changes);
    }
}

/// プログラムのエントリポイント。
///
/// Tokio のランタイム上で動作し、標準入出力を通じて LSP クライアントと通信します。
#[tokio::main]
async fn main() {
    // tracing の初期化
    let logger = Logger::new();

    // 標準入力／出力を LSP の通信チャネルとして利用
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // 環境変数の初期化
    let current_exe = std::env::current_exe().unwrap(); // "C:\\Users\\talos\\RustroverProjects\\fifty_four\\target\\debug\\fifty_four_lsp.exe"
    let path = current_exe.ancestors().nth(3).unwrap(); // "C:\\Users\\talos\\RustroverProjects\\fifty_four"

    if let Err(err) = dotenvx_rs::dotenvx::from_path(path.join(".env")) {
        eprintln!("Failed to load environment variables: {}", err);

        eprintln!("{:?}", std::env::vars());
        return;
    }

    // DBマイグレーション
    let conn = std::sync::Mutex::new(
        rusqlite::Connection::open(path.join("data").join("fifty_four.db")).unwrap(),
    );
    {
        let mut c = conn.lock().unwrap();
        match migrations::migrations::runner().run(c.deref_mut()) {
            Ok(_) => {}
            Err(e) => {
                panic!("Fail to migrate: {:?}", e);
            }
        }
    }

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    info!("initialize lsp service");
    let (service, socket) = tower_lsp::LspService::build(|client| Backend {
        client,
        text: DashMap::new(),
        llm: Some(tokio::sync::Mutex::new(Box::new(
            GenericLlmClient::from_name(
                // "anthropic/claude-sonnet-4-6",
                "google/gemini-3.1-flash-lite-preview",
            ),
            // GenericLlmClient::new(
            //     llm::Provider::LMStudio,
            //     "lfm2.5-1.2b-jp",
            //     Some("http://localhost:1234/v1/"),
            // ),
        ))),
        data_path: path.join("data"),
        conn: conn,
    })
    .finish();

    // サーバを起動してクライアントとのメッセージループを開始する
    info!("start server");

    tower_lsp::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;

    drop(logger);
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        }
    }

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(|l| l.to_string()).collect()
    }

    // 1. 単一文字挿入
    #[test]
    fn test_insert_char() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(&mut ls, &[change(Some(range(0, 19, 0, 19)), "!")]);
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。!
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ))
        );
    }

    // 2. 複数行の削除（text が空）
    #[test]
    fn test_delete_lines() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。
            驕れる人も久しからず、ただ春の夜の夢のごとし。
            猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
        ));
        apply_changes(&mut ls, &[change(Some(range(1, 0, 3, 0)), "")]);
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
            ))
        );
    }

    // 3. 1行を複数行に置換
    #[test]
    fn test_replace_line_with_multiline() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。
            驕れる人も久しからず、ただ春の夜の夢のごとし。"
        ));
        apply_changes(
            &mut ls,
            &[change(
                Some(range(1, 0, 1, 3)),
                indoc!(
                    "猛き者もつひにはほろびぬ、
                    ひとへに風の前の塵に同じ。"
                ),
            )],
        );
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                猛き者もつひにはほろびぬ、
                ひとへに風の前の塵に同じ。樹の花の色、盛者必衰の理をあらはす。
                驕れる人も久しからず、ただ春の夜の夢のごとし。"
            ))
        );
    }

    // 4. 行頭への挿入
    #[test]
    fn test_insert_at_line_start() {
        let mut ls = lines("諸行無常の響きあり。");
        apply_changes(
            &mut ls,
            &[change(Some(range(0, 0, 0, 0)), "祇園精舍の鐘の声、")],
        );
        assert_eq!(ls, lines("祇園精舍の鐘の声、諸行無常の響きあり。"));
    }

    // 5. 行末への挿入（改行を追加）
    #[test]
    fn test_insert_newline_at_end() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(
            &mut ls,
            &[change(
                Some(range(0, 19, 0, 19)),
                "\nただ春の夜の夢のごとし。",
            )],
        );
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                ただ春の夜の夢のごとし。
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ))
        );
    }

    // 6. range が None → フルテキスト置換
    #[test]
    fn test_full_replace_when_range_none() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(
            &mut ls,
            &[change(
                None,
                indoc!(
                    "驕れる人も久しからず、ただ春の夜の夢のごとし。
                    猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
                ),
            )],
        );
        assert_eq!(
            ls,
            lines(indoc!(
                "驕れる人も久しからず、ただ春の夜の夢のごとし。
                猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
            ))
        );
    }

    // 7. 複数 change の順次適用
    #[test]
    fn test_multiple_changes_applied_sequentially() {
        let mut ls = lines("abc");
        apply_changes(
            &mut ls,
            &[
                change(Some(range(0, 3, 0, 3)), "d"),
                change(Some(range(0, 0, 0, 1)), "A"),
            ],
        );
        assert_eq!(ls, lines("Abcd"));
    }
}
