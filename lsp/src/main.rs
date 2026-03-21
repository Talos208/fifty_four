// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use tower_lsp::jsonrpc::{ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, async_trait};
// use tracing::{debug, info, instrument, span, warn};
mod highlight;
use crate::highlight::Highlighter;
mod logging;
mod types;
use crate::types::{CursorContext, LineData};
use dashmap::DashMap;
use std::collections::HashMap;
use std::str::FromStr;
mod cursor_context;
mod llm;
use crate::llm::{Content, LlmClient, LlmClientBuilder};
use std::panic;
use std::path::Path;
mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}
use genai::chat::{ReasoningEffort, ServiceTier, Verbosity};
use indoc::indoc;
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use rust_embed::Embed;
use std::ops::DerefMut;

/// 直近のcompletion候補を記録する構造体（デバッグビルドのみ）
#[cfg(debug_assertions)]
#[derive(Debug)]
struct PendingCandidate {
    db_id: i64,
    candidate: String,
}

/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
    // 文章データ（uri、行ごとのテキスト）
    text: DashMap<String, Vec<LineData>>,
    // LLMクライアントへのハンドル
    llm: tokio::sync::Mutex<Option<Box<dyn LlmClient>>>,

    highliter: Highlighter,
    // 記録用DBへのコネクション
    #[cfg(debug_assertions)]
    conn: Option<std::sync::Mutex<rusqlite::Connection>>,
    // 直近のcompletion候補（URI, 候補リスト）
    #[cfg(debug_assertions)]
    pending_completions: std::sync::Mutex<Option<(String, Vec<PendingCandidate>)>>,
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

        if let Some(opt) = _param.initialization_options
            && let Some(llm) = opt.get("llm")
        {
            // LLMクライアントを初期化
            let mut builder = LlmClientBuilder::from_value(llm);

            llm.get("model").inspect(|v| {
                builder.model(v.as_str().unwrap());
            });

            llm.get("url").inspect(|v| {
                builder.url(v.as_str().unwrap());
            });

            let cl = builder.build();

            debug!(
                "LLM built.\tmodel: {:?}\n\tservice_target: {}",
                cl,
                cl.get_service_target().await
            );

            self.llm.lock().await.replace(cl);
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
           info!("did_change_configuration: {:?}", param.settings);

           // エディタ側の設定を読む
           let params = vec![ConfigurationItem {
               scope_uri: None,
               section: Some("settings".to_string()),
           }];

           debug!("client.configuration");
           let _ = self.client.configuration(params).await.map(|i| {
               i.iter().inspect(|j| {
                   debug!("\t{:?}", j);
               });
           });
       }
    */
    // #[instrument(ret)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("file opened!");

        // 行ごとに分割しておく
        let texts = params
            .text_document
            .text
            .lines() // TODO これは大丈夫な気がするけど……
            .map(|s| s.to_string())
            .collect();
        self.update_all(params.text_document.uri.as_str(), 0, texts);

        let _ = self.client.semantic_tokens_refresh().await;
    }

    // #[instrument]
    async fn did_change(&self, param: DidChangeTextDocumentParams) {
        debug!("did_change");

        #[cfg(debug_assertions)]
        {
            let uri = param.text_document.uri.as_str();
            let taken = self.pending_completions.lock().unwrap().take();
            if let Some((pending_uri, candidates)) = taken
                && pending_uri == uri
            {
                for change in &param.content_changes {
                    if let Some(c) = candidates.iter().find(|c| c.candidate == change.text) {
                        if let Some(db) = self.conn.as_ref() {
                            let db = db.lock().unwrap();
                            if let Err(e) = db.execute(
                                "UPDATE completion_candidates SET selected = true WHERE id = ?;",
                                rusqlite::params![c.db_id],
                            ) {
                                debug!("Failed to update completion_candidates: {}", e);
                            }
                        }
                        break;
                    }
                }
            }
        }

        // 全体が送られて来た時
        if param.content_changes.iter().all(|c| c.range.is_none()) {
            self.update_all(
                param.text_document.uri.as_str(),
                0,
                param
                    .content_changes
                    .iter()
                    .flat_map(|c| c.text.lines().map(|s| s.to_string()))
                    .collect(),
            );
            return;
        }

        debug!("param {:?}", param);

        self.update_partial(
            param.text_document.uri.as_str(),
            param
                .content_changes
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
            param
                .content_changes
                .iter()
                .map(|c| c.range.unwrap())
                .collect::<Vec<_>>()
                .as_slice(),
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

        let uri = params.text_document.uri.as_ref();
        let mut lines = self.text.get(uri).expect("Failed to get text").to_vec();

        self.highliter.initialize();

        let tokens: Vec<_> = lines
            .iter_mut()
            .map(|l| self.highliter.tokenize(l))
            .collect();

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
        let mut text = self.text.get(uri).unwrap().value().to_owned();
        let line_no = params.text_document_position.position.line as usize;
        let offset = params.text_document_position.position.character as usize;

        // 対象行と前後をtokenize
        self.highliter.initialize(); // TODO 正しいdepthを割り当てたい
        let before = text
            .iter()
            .take(line_no)
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if offset > 0 && before.is_empty() {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "offset > 0 && before.is_empty()",
            ));
        }

        let line: &str = text[line_no].text.as_str();
        let left: String = line.chars().take(offset).collect();

        // カーソルコンテキスト分類
        let context = cursor_context::classify_complesion_mode(&mut text, line_no, offset, |ln| {
            self.tokenize_line(uri, ln);
        });
        debug!("CursorContext: {:?}", context);

        let prompt_fn = Backend::ctx_to_prompt_name(context);
        debug!("Prompt: {}", prompt_fn);
        let prompt = String::from_utf8_lossy(
            Asset::get(prompt_fn)
                .unwrap_or_else(|| panic!("{} not found", prompt_fn))
                .data
                .as_ref(),
        )
        .to_string();

        // front matter処理
        let options = if let Ok(path) = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            && let Some(ext) = path.extension()
            && ext.to_string_lossy() == ".md"
        {
            let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
            let parsed_matter = matter.parse::<HashMap<String, String>>(prompt.as_str());
            parsed_matter.map(|entry| entry.data).unwrap()
        } else {
            None
        };

        let mut completion_id = 0u32;
        let raw = self
                .use_llm(
                    async |l| {
                        if let Some(data) = options {
                            if let Some(v) = data.get("max_tokens") && let Ok(n) = v.parse::<u32>() { l.max_tokens(n); }
                            if let Some(v) = data.get("temperature") && let Ok(n) = v.parse::<f64>() { l.temperature(n); }
                            if let Some(v) = data.get("top_p") && let Ok(n) = v.parse::<f64>() { l.top_p(n); }
                            if let Some(v) = data.get("stop_sequences") { l.stop_sequences(v.split(',').map(|s| s.to_string()).collect()); }
                            if let Some(v) = data.get("seed") && let Ok(n) = v.parse::<u64>() { l.seed(n); }
                            if let Some(v) = data.get("reasoning_effort") && let Ok(n) = v.parse::<ReasoningEffort>() { l.reasoning_effort(n); }
                            // if let Some(v) = data.get("response_format") { ... }
                            if let Some(v) = data.get("service_tier") && let Ok(n) = v.parse::<ServiceTier>() { l.service_tier(n); }
                            if let Some(v) = data.get("verbosity") && let Ok(n) = v.parse::<Verbosity>() { l.verbosity(n); }
                        }

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

                #[cfg(debug_assertions)]
                let mut pending: Vec<PendingCandidate> = Vec::new();

                let items = response
                    .lines()
                    .map(|r| {
                        let sr = match context {
                            CursorContext::BeforeClosingBracket => {
                                if !r.starts_with('。') {
                                    "。".to_string() + r
                                } else if r.ends_with('。') {
                                    r.strip_suffix("。").unwrap_or(r).to_string()
                                } else {
                                    r.to_string()
                                }
                            }
                            CursorContext::EmptyBracket => {
                                if r.ends_with('。') {
                                    r.strip_suffix("。").unwrap_or(r).to_string()
                                } else {
                                    r.to_string()
                                }
                            }
                            CursorContext::AfterClosingBracket => "\n".to_string() + r,
                            _ => {
                                if !r.ends_with('。') {
                                    r.to_string() + "。"
                                } else {
                                    r.to_string()
                                }
                            }
                        };

                        #[cfg(debug_assertions)]
                        {
                            if let Some(db) = self.conn.as_ref() {
                                let db = db.lock().unwrap();
                                match db.query_row(
                                    indoc!(
                                        "INSERT INTO completion_candidates
                                            (completion_id, rank, candidate)
                                            VALUES (?,?,?) RETURNING id;"
                                    ),
                                    rusqlite::params![completion_id, 0, sr],
                                    |row| row.get::<_, i64>(0),
                                ) {
                                    Ok(id) => pending.push(PendingCandidate {
                                        db_id: id,
                                        candidate: r.to_string(),
                                    }),
                                    Err(err) => {
                                        debug!("Failed to insert completion_candidate: {}", err)
                                    }
                                }
                            }
                        }
                        if sr.chars().count() > 25 {
                            CompletionItem {
                                label: sr.chars().take(23).collect::<String>() + "……",
                                kind: Some(CompletionItemKind::TEXT),
                                documentation: Some(Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: sr.clone(),
                                })),
                                insert_text: Some(sr),
                                ..Default::default()
                            }
                        } else {
                            CompletionItem {
                                label: sr,
                                kind: Some(CompletionItemKind::TEXT),
                                ..Default::default()
                            }
                        }
                    })
                    .collect();

                #[cfg(debug_assertions)]
                {
                    *self.pending_completions.lock().unwrap() = Some((uri.to_string(), pending));
                }
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

fn apply_changes<T: AsRef<str>>(lines: &mut Vec<LineData>, text: T, range: Range) {
    let start_line = range.start.line as usize;
    let start_char = range.start.character as usize;
    let end_line = range.end.line as usize;
    let end_char = range.end.character as usize;

    let prefix = lines
        .get(start_line)
        .map(|l| {
            let ix = start_char.min(l.text.len());
            let n = l.text.chars().take(ix);
            String::from_iter(n)
        })
        .unwrap_or("".to_string());
    let suffix = lines
        .get(end_line)
        .map(|l| {
            let ix = end_char.min(l.text.len());
            let n = l.text.chars().skip(ix);
            String::from_iter(n)
        })
        .unwrap_or("".to_string());

    let mut new_text = String::with_capacity(prefix.len() + text.as_ref().len() + suffix.len());
    new_text.push_str(&prefix);
    new_text.push_str(text.as_ref());
    new_text.push_str(&suffix);

    let cr = regex::Regex::new(r"(\r\n|\n)").unwrap();
    let new_lines: Vec<_> = cr
        .split(new_text.as_str())
        // .lines() // 文字列末尾に改行だけ挿入した場合、1行扱いになってしまう
        .map(|s| LineData::from_str(s).unwrap())
        .collect();

    let end = end_line.min(lines.len() - 1);
    lines.splice(start_line..=end, new_lines);
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
        self.text.insert(
            uri.to_string(),
            texts
                .iter()
                .map(|t| LineData::from_str(t).unwrap())
                .collect::<Vec<_>>(),
        );
    }

    fn update_partial(&self, uri: &str, texts: &[impl AsRef<str>], changes: &[Range]) {
        if !self.text.contains_key(uri) {
            return;
        }

        let mut rv = self.text.get_mut(uri).unwrap();

        texts
            .iter()
            .zip(changes)
            .rev() // 後ろから突っ込んで、改行による行数の変更で矛盾が生じないように
            .for_each(|(text, change)| {
                apply_changes(rv.deref_mut(), text, *change);
            });
    }

    fn ctx_to_prompt_name(ctx: CursorContext) -> &'static str {
        match ctx {
            CursorContext::AfterClosingBracket => "prompt_completion_after_bracket.md",
            CursorContext::AfterSentenceEnd => "prompt_completion_after_sentence.md",
            CursorContext::BeforeClosingBracket => "prompt_completion_before_bracket.md",
            CursorContext::EmptyBracket => "prompt_completion_empty_bracket.md",
            CursorContext::InBracketOther => "prompt_completion_in_bracket.md",
            CursorContext::Other => "prompt_completion.md",
        }
    }

    pub fn tokenize_line(&self, url: &str, line_no: usize) {
        if !self.text.get(url).unwrap()[line_no].tokens.is_empty() {
            return;
        }
        self.highliter.tokenize(
            self.text
                .get_mut(&url.to_string())
                .unwrap()
                .value_mut()
                .get_mut(line_no)
                .unwrap(),
        );
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
        if let Err(err) = dotenvx_rs::dotenvx::from_path(path.join(".env")) {
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
        #[cfg(debug_assertions)]
        pending_completions: std::sync::Mutex::new(None),
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
    use tower_lsp::lsp_types::{Position, Range};

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

    fn lines(s: &str) -> Vec<LineData> {
        s.lines().map(|l| LineData::from_str(l).unwrap()).collect()
    }

    // 1. 単一文字挿入
    #[test]
    fn test_insert_char() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(&mut ls, "!", range(0, 19, 0, 19));
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
        apply_changes(&mut ls, "", range(1, 0, 3, 0));
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
            indoc!(
                "猛き者もつひにはほろびぬ、
                ひとへに風の前の塵に同じ。"
            ),
            range(1, 0, 1, 3),
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
        apply_changes(&mut ls, "祇園精舍の鐘の声、", range(0, 0, 0, 0));
        assert_eq!(ls, lines("祇園精舍の鐘の声、諸行無常の響きあり。"));
    }

    // 5. 行末への挿入（改行を追加）
    #[test]
    fn test_insert_newline_at_end() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(&mut ls, "\nただ春の夜の夢のごとし。", range(0, 19, 0, 19));
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                ただ春の夜の夢のごとし。
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ))
        );
    }

    // 6.tokenize後の行挿入
    #[test]
    fn test_insert_after_tokenize() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            「娑羅双樹」の花の色、「盛者必衰」の理をあらはす。"
        ));
        let hl = Highlighter::new();
        hl.initialize();
        ls.iter_mut().for_each(|l| {
            hl.tokenize(l);
        });

        assert_eq!(ls.len(), 2);
        // let mut s = String::new();
        // ls[1].tokens.iter().for_each(|t| {
        //     let _ = writeln!(s, "{:?}:\t{:?}", ls[1].surface(&t), t.details.split_at(4).0);
        // });
        /*
        "「":	["記号", "括弧開", "*", "*"]
        "娑":	["名詞", "一般", "*", "*"]
        "羅":	["名詞", "固有名詞", "人名", "姓"]
        "双":	["接頭詞", "名詞接続", "*", "*"]
        "樹":	["名詞", "一般", "*", "*"]
        "」":	["記号", "括弧閉", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "花":	["名詞", "一般", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "色":	["名詞", "一般", "*", "*"]
        "、":	["記号", "読点", "*", "*"]
        "「":	["記号", "括弧開", "*", "*"]
        "盛者":	["名詞", "一般", "*", "*"]
        "必":	["名詞", "一般", "*", "*"]
        "衰":	["名詞", "一般", "*", "*"]
        "」":	["記号", "括弧閉", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "理":	["名詞", "一般", "*", "*"]
        "を":	["助詞", "格助詞", "一般", "*"]
        "あら":	["名詞", "一般", "*", "*"]
        "は":	["助詞", "係助詞", "*", "*"]
        "す":	["動詞", "自立", "*", "*"]
        "。":	["記号", "句点", "*", "*"]
        */
        assert_eq!(ls[1].tokens.len(), 23);

        // 一文字追加
        apply_changes(&mut ls, "\r\n", range(0, 19, 0, 19));

        assert_eq!(ls.len(), 3); // 改行分だけ行数が増える
        assert_eq!(ls[1].text, "");

        assert_ne!(ls[0].text.len(), 0); // 変更された行もtextはそのまま
        assert_eq!(ls[0].tokens.len(), 0); // 変更された行のtokenは空になる

        assert_eq!(ls[2].tokens.len(), 23); // 改行分後ろにずれるけどtokenはそのまま

        // let mut s = String::new();
        // ls[1].tokens.iter().for_each(|t| {
        //     let _ = writeln!(s, "{:?}:\t{:?}", ls[1].surface(&t), t.details.split_at(4).0);
        // });
    }
    /*
    size=2*/
    // 6. range が None → フルテキスト置換
    /*   #[test]
        fn test_full_replace_when_range_none() {
            let mut ls = lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ));
            apply_changes(
                &mut ls,
                indoc!(
                    "驕れる人も久しからず、ただ春の夜の夢のごとし。
                    猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
                ),
                &[change(
                    None,
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
    */
}
