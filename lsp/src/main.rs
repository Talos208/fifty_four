// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use tower_lsp::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, async_trait};
// use tracing::{debug, info, instrument, span, warn};
mod highlight;
use crate::highlight::Highlighter;
mod logging;
use crate::logging::Logger;
use dashmap::DashMap;
mod llm;
use crate::llm::{Content, LlmClient, LlmClientBuilder};
use rusqlite;
use std::panic;
use std::path::{Display, Path, PathBuf};
mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}
use env_logger;
use indoc::{formatdoc, indoc};
use log::{debug, error, info, warn};
use regex::Regex;
use rust_embed::Embed;
use std::ops::{Deref, DerefMut};
/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
    // 文章データ（uri、行ごとのテキスト）
    text: DashMap<String, Vec<String>>,
    // LLMクライアントへのハンドル
    llm: tokio::sync::Mutex<Option<Box<dyn LlmClient>>>,

    highliter: Highlighter,
    // 記録用DBへのコネクション
    #[cfg(debug_assertions)]
    conn: Option<std::sync::Mutex<rusqlite::Connection>>,
}

/// `LanguageServer` トレイトの実装。
///
/// ここでは最小限のメソッドのみ実装しており、将来的にホバーや補完などを追加できます。
#[async_trait]
impl LanguageServer for Backend {
    /// LSP クライアントからの `initialize` リクエストに応答します。
    ///
    /// 返却する `InitializeResult` でサーバの機能（capabilities）をクライアントに伝えます。
    // #[instrument(ret, err)]
    async fn initialize(&self, _param: InitializeParams) -> Result<InitializeResult> {
        // サーバの機能（capabilities）を構成します。
        // ここでは最小限として semanticTokens の提供（空実装）を宣言します。

        debug!("Workspace: {:?}", _param.workspace_folders);

        if let Some(info) = _param.client_info {
            debug!("Client_info: {:?}", info);
        }

        if let Some(opt) = _param.initialization_options {
            if let Some(llm) = opt.get("llm") {
                // LLMクライアントを初期化
                let mut builder = LlmClientBuilder::from_value(llm);

                llm.get("model").and_then(|v| {
                    builder.model(v.as_str().unwrap());
                    Some(v)
                });

                llm.get("url").and_then(|v| {
                    builder.url(v.as_str().unwrap());
                    Some(v)
                });

                let cl = builder.build();

                debug!(
                    "LLM built.\tmodel: {:?}\n\tservice_target: {}",
                    cl,
                    cl.get_service_target().await
                );

                self.llm.lock().await.replace(cl);
            }
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
                                // LSP 3.17 仕様の SemanticTokenTypes 定義順
                                token_types: vec![
                                    SemanticTokenType::NAMESPACE,
                                    SemanticTokenType::TYPE,
                                    SemanticTokenType::CLASS,
                                    SemanticTokenType::ENUM,
                                    SemanticTokenType::INTERFACE,
                                    SemanticTokenType::STRUCT,
                                    SemanticTokenType::TYPE_PARAMETER,
                                    SemanticTokenType::PARAMETER,
                                    SemanticTokenType::VARIABLE,
                                    SemanticTokenType::PROPERTY,
                                    SemanticTokenType::ENUM_MEMBER,
                                    SemanticTokenType::EVENT,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::METHOD,
                                    SemanticTokenType::MACRO,
                                    SemanticTokenType::KEYWORD,
                                    SemanticTokenType::MODIFIER,
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                    SemanticTokenType::REGEXP,
                                    SemanticTokenType::OPERATOR,
                                    SemanticTokenType::DECORATOR,
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
                    label_details_support: Some(true),
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
    // #[instrument(ret)]
    async fn initialized(&self, _: InitializedParams) {
        debug!("LSP server initialized");

        let req = vec![ConfigurationItem {
            scope_uri: None,
            section: None,
        }];
        let res = self.client.configuration(req).await.unwrap();
        debug!("{:?}", res);
    }
    /*
       #[instrument(ret)]
       async fn did_change_configuration(&self, param: DidChangeConfigurationParams) {
           self.client
               .log_message(MessageType::INFO, "did_change_configuration")
               .await;
           self.client
               .log_message(
                   MessageType::INFO,
                   format!("{:?}", param.settings.as_object()),
               )
               .await;

           // エディタ側の設定を読む
           /*
           let params = vec![ConfigurationItem {
               scope_uri: None,
               section: Some("settings".to_string()),
           }];

           let mut msg = vec![];
           let _ = self.client.configuration(params).await.map(|i| {
               i.iter().for_each(|j| {
                   msg.push(format!("{:?}", j));
                   ()
               });
           });
           self.client
               .log_message(MessageType::INFO, msg.join("\n"))
               .await;
               */
       }
    */
    // #[instrument(ret)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("file opened!");

        // 行ごとに分割しておく
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        let texts = cr
            .split(params.text_document.text.as_str())
            .map(|s| s.to_string())
            .collect();
        self.update_all(params.text_document.uri.as_str(), 0, texts);

        let _ = self.client.semantic_tokens_refresh().await;
    }

    // #[instrument]
    async fn did_change(&self, param: DidChangeTextDocumentParams) {
        debug!("did_change");

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

        debug!("param {:?}", param);

        self.update_partial(
            param.text_document.uri.as_str(),
            param.content_changes.as_ref(),
        );

        let _ = self.client.semantic_tokens_refresh().await;
    }

    // #[instrument(ret)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("file closed!");

        self.text.remove(params.text_document.uri.as_str());
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    // #[instrument(ret, err)]
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// ドキュメント全体に対する semantic tokens の問い合わせに応答します。
    ///
    // #[instrument(ret, err)]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        debug!("semantic_token_full");

        let text = self
            .text
            .get(params.text_document.uri.as_ref())
            .expect("Failed to get text")
            .value()
            .to_vec();

        self.highliter.initialize();

        let tokens = text.iter().map(|s| self.highliter.tokenize(s));

        let vec = Highlighter::to_semantic_tokens(tokens);

        let tokens = SemanticTokens {
            result_id: None,
            data: vec,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    // #[instrument(ret, err)]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        debug!("completion");

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
        let text = self.text.get(uri).unwrap().value().to_owned();
        let line_no = params.text_document_position.position.line as usize;
        let offset = params.text_document_position.position.character as usize;

        let before = text
            .iter()
            .take(line_no)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // self.client
        //     .log_message(MessageType::LOG, before.as_str())
        //     .await;

        let line: &str = text[line_no].as_ref();
        let left: String = line.chars().take(offset).collect();

        // self.client
        //     .log_message(MessageType::LOG, left.as_str())
        //     .await;

        let prompt = String::from_utf8_lossy(
            Asset::get("prompt_completion.md")
                .expect("prompt_completion.md not found")
                .data
                .as_ref(),
        )
        .to_string();

        if offset > 0 || !before.is_empty() {
            let mut completion_id = 0u32;
            let raw = self
                .use_llm(
                    async |l| {
                        l.add(Content::Text(prompt));
                        l.add(Content::Text(before));
                        l.add(Content::Text(left));

                        #[cfg(debug_assertions)]
                        {
                            if let Some(db) = self.conn.as_ref() {
                                let db = db.lock().unwrap();
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
                                    Err(e) => error!("Failed to insert completion: {:?}", e),
                                };
                            }
                        }

                        debug!("Before chat.");
                        l.chat().await
                    },
                )
                .await;

            debug!(
                "{}",
                format!("{:?}", raw).chars().take(30).collect::<String>()
            );
            match raw {
                Ok(response) => {
                    debug!("raw Ok.");
                    let cr = Regex::new(r"\r\n|\r|\n").unwrap();

                    let items = cr
                        .split(response.as_str())
                        .inspect(|i| {
                            #[cfg(debug_assertions)]
                            {
                                if let Some(db) = self.conn.as_ref() {
                                    let db = db.lock().unwrap();
                                    db.execute(
                                        indoc!(
                                            "INSERT INTO completion_candidates
                                        (completion_id, rank, candidate)
                                        VALUES (?,?,?);"
                                        ),
                                        rusqlite::params![completion_id, 0, i],
                                    )
                                    .unwrap_or_else(|err| {
                                        debug!("Failed to insert completion_candidate: {}", err);
                                        0
                                    });
                                }
                            }
                        })
                        .map(|r| {
                            if r.len() > 20 {
                                CompletionItem {
                                    label: r.chars().take(18).collect::<String>() + "…",
                                    kind: Some(CompletionItemKind::TEXT),
                                    documentation: Some(Documentation::MarkupContent(
                                        MarkupContent {
                                            kind: MarkupKind::Markdown,
                                            value: r.to_string(),
                                        },
                                    )),
                                    insert_text: Some(r.to_string()),
                                    ..Default::default()
                                }
                            } else {
                                CompletionItem {
                                    label: r.to_string(),
                                    kind: Some(CompletionItemKind::TEXT),
                                    ..Default::default()
                                }
                            }
                        })
                        .collect();
                    let list = CompletionList {
                        is_incomplete: false,
                        items,
                    };

                    Ok(Some(CompletionResponse::List(list)))
                }
                Err(err) => {
                    error!("Error on completion: {:?}", err);
                    Err(tower_lsp::jsonrpc::Error::invalid_params(err.to_string()))
                }
            }
        } else {
            Err(tower_lsp::jsonrpc::Error::invalid_params(
                "offset <= 0 && before.is_empty()",
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
        let mut ref_llm = self.llm.lock().await;
        if let Some(llm) = ref_llm.deref_mut() {
            debug!("Before use_llm.");
            let ret = proc(llm).await;
            debug!("After use_llm.");
            return ret;
        }

        core::result::Result::Err(Box::new(tower_lsp::jsonrpc::Error {
            code: ErrorCode::ServerError(-32002),
            message: std::borrow::Cow::Borrowed("LLM not initialized"),
            data: None,
        }))
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

#[derive(Embed)]
#[folder = "../data/"]
struct Asset;

/// プログラムのエントリポイント。
///
/// Tokio のランタイム上で動作し、標準入出力を通じて LSP クライアントと通信します。
#[tokio::main]
async fn main() {
    // tracing の初期化
    // let logger = Logger::new();

    // 標準入力／出力を LSP の通信チャネルとして利用
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // 環境変数の初期化
    #[cfg(debug_assertions)]
    {
        // こやつパス間違っててもエラーを返さない
        if let Err(err) = dotenvx_rs::dotenvx::from_path(&path.join(".env")) {
            eprintln!("Failed to load environment variables: {}", err);

            eprintln!("{:?}", std::env::vars());
            return;
        }
    }

    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    // DBマイグレーション
    #[cfg(debug_assertions)]
    let conn = {
        let mut c = rusqlite::Connection::open(path.join("data").join("fifty_four.db"))
            .expect("Fail to open database");
        match migrations::migrations::runner().run(&mut c) {
            Ok(_) => {}
            Err(e) => {
                panic!("Fail to migrate: {:?}", e);
            }
        }
        Some(std::sync::Mutex::new(c))
    };
    #[cfg(not(debug_assertions))]
    let conn = None;

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    info!("initialize lsp service");
    let none: Option<Box<dyn LlmClient>> = None;
    let (service, socket) = tower_lsp::LspService::build(|client| Backend {
        client,
        text: DashMap::new(),
        llm: tokio::sync::Mutex::new(none),
        highliter: Highlighter::new(),
        conn,
    })
    .finish();

    // サーバを起動してクライアントとのメッセージループを開始する
    info!("start server");

    tower_lsp::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;

    // drop(logger);
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
