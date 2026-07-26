//! LSP ハンドラ本体。
//!
//! `main.rs` から切り出したモジュール。`Backend` はサーバの状態を保持し、
//! `LanguageServer` トレイトの実装として各種 LSP リクエスト/通知に応答する。
//! 構築は `Backend::new` に集約し、フィールドは private のまま保つ
//! (`main()` からは構造体リテラルではなくこのコンストラクタ経由で初期化する)。

use crate::character::{
    CharacterCache, FileCacheEntry, collect_allowed_names, lookup_character_markdown,
    parse_all_content,
};
use crate::character_updater::UpdateState;
use crate::flight_recorder::{FlightRecorder, PendingCandidate};
use crate::highlight::Highlighter;
use crate::llm::{Content, LlmClientBuilder, LlmError, LlmInterface, ModelCapability};
use crate::progress::CompletionProgress;
use crate::text::{apply_changes, precursor_word, shorten};
use crate::types::{CursorContext, LineData};
use dashmap::DashMap;
use dashmap::mapref::one::RefMut;
use dashmap::try_result::TryResult;
use genai::chat::{ChatResponseFormat, JsonSpec, ReasoningEffort, ServiceTier, Verbosity};
use std::collections::HashMap;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer, UriExt};

/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
pub(crate) struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
    // 文章データ（uri、行ごとのテキスト）
    text: DashMap<String, Vec<LineData>>,
    //ワークスペース
    workspace: Arc<tokio::sync::Mutex<Vec<PathBuf>>>,
    // 文章補完用 LLM(Arc化: 周期タスクに clone して渡せるようにする)
    llm: Arc<tokio::sync::Mutex<Option<Box<dyn LlmInterface>>>>,
    // バックグラウンド長考タスク用 LLM(補完をブロックしない。現状はキャラ設定収集が利用)
    background_llm: Arc<tokio::sync::Mutex<Option<Box<dyn LlmInterface>>>>,
    // false にするとキャラクター設定の自動更新タスクを起動しない
    character_updater_enabled: std::sync::atomic::AtomicBool,
    character_updater_min_chars: std::sync::atomic::AtomicUsize,
    character_updater_max_chars: std::sync::atomic::AtomicUsize,
    character_updater_idle_secs: std::sync::atomic::AtomicU64,

    highlighter: Highlighter,
    // デバッグビルド専用のDB操作
    db: Arc<FlightRecorder>,
    // キャラクター設定ファイルのパース結果キャッシュ
    character_cache: CharacterCache,
    // URI ごとのキャラクター更新トリガー状態
    update_states: DashMap<String, Arc<parking_lot::Mutex<UpdateState>>>,
    // クライアントが window/workDoneProgress をサポートするか(initialize で判定)
    work_done_progress_supported: std::sync::atomic::AtomicBool,
}

fn build_llm_client(cfg: &serde_json::Value) -> Box<dyn LlmInterface> {
    let sys_prompt = crate::assets::load("system.md");
    if sys_prompt.is_none() {
        warn!("system.md not found on disk nor in embedded assets; LLM runs without system prompt");
    }
    LlmClientBuilder::from_value(cfg)
        .sys_prompt(sys_prompt)
        .build()
}

/// `LanguageServer` トレイトの実装。
///
/// ここでは最小限のメソッドのみ実装しており、将来的にホバーや補完などを追加できます。
impl LanguageServer for Backend {
    /// LSP クライアントからの `initialize` リクエストに応答します。
    ///
    /// 返却する `InitializeResult` でサーバの機能（capabilities）をクライアントに伝えます。
    // #[instrument(ret, err)]
    async fn initialize(
        &self,
        _param: InitializeParams,
    ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
        // サーバの機能（capabilities）を構成します。
        // ここでは最小限として semanticTokens の提供（空実装）を宣言します。
        debug!("initialize");

        debug!("Workspace: {:?}", _param.workspace_folders);
        if let Some(ws) = _param.workspace_folders {
            self.init_workspace(ws).await;
        }

        if let Some(info) = _param.client_info {
            debug!("Client_info: {:?}", info);
        }

        // クライアントの workDoneProgress 対応を記録する(completion 中の進捗表示に使う)。
        let wdp_supported = _param
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        self.work_done_progress_supported
            .store(wdp_supported, std::sync::atomic::Ordering::Relaxed);
        debug!("client workDoneProgress support: {}", wdp_supported);

        if let Some(opt) = _param.initialization_options {
            debug!("initialization_options: {:?}", opt);
            if let Some(cu) = opt.get("character_updater") {
                debug!("character_updater config found: {:?}", cu);
                if cu.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                    self.character_updater_enabled
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(v) = cu.get("min_chars").and_then(|v| v.as_u64()) {
                    self.character_updater_min_chars
                        .store(v as usize, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(v) = cu.get("max_chars").and_then(|v| v.as_u64()) {
                    self.character_updater_max_chars
                        .store(v as usize, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(v) = cu.get("idle_timeout_secs").and_then(|v| v.as_u64()) {
                    self.character_updater_idle_secs
                        .store(v, std::sync::atomic::Ordering::Relaxed);
                }
            } else {
                debug!("no character_updater config; using defaults");
            }
            // 反映後の実効設定を出力(デフォルト/設定値どちらが効いているか確認用)
            debug!(
                "character_updater effective: enabled={} min_chars={} max_chars={} idle_secs={}",
                self.character_updater_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.character_updater_min_chars
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.character_updater_max_chars
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.character_updater_idle_secs
                    .load(std::sync::atomic::Ordering::Relaxed),
            );

            if let Some(llm_root) = opt.get("llm") {
                // 旧形式互換: llm:{provider,...} → ondemand として扱う。
                // 新形式は llm:{ondemand:{...}, deferred:{...}}。
                let ondemand_cfg = llm_root.get("ondemand").or_else(|| {
                    if llm_root.get("provider").is_some() {
                        Some(llm_root)
                    } else {
                        None
                    }
                });

                if let Some(cfg) = ondemand_cfg {
                    let cl = build_llm_client(cfg);
                    debug!(
                        "llm.ondemand built.\tmodel: {:?}\n\tservice_target: {}",
                        cl,
                        cl.get_service_target().await
                    );
                    self.llm.lock().await.replace(cl);
                } else {
                    warn!(
                        "llm config has neither ondemand nor provider key; completion LLM remains uninitialized"
                    );
                }
                let deferred_cfg = llm_root.get("deferred");
                if deferred_cfg.is_none() {
                    // フォールバック自体は許容するが、バックグラウンド処理が
                    // ondemand の LLM で動くことに気づけるよう warn を出す。
                    warn!(
                        "llm.deferred is not configured; background LLM (character_updater) falls back to the ondemand config"
                    );
                }
                if let Some(cfg) = deferred_cfg
                    .or_else(|| llm_root.get("ondemand"))
                    .or_else(|| {
                        if llm_root.get("provider").is_some() {
                            Some(llm_root)
                        } else {
                            None
                        }
                    })
                {
                    let cl = build_llm_client(cfg);
                    debug!(
                        "llm.deferred built.\tmodel: {:?}\n\tservice_target: {}",
                        cl,
                        cl.get_service_target().await
                    );
                    self.background_llm.lock().await.replace(cl);
                }
            }
        }

        let capabilities = ServerCapabilities {
            // UTF-16 は LSP の必須ベースラインで全クライアントがサポートするため、
            // クライアントの general.positionEncodings を確認せず常に宣言してよい。
            // (実クライアントの Zed も utf-16 のみをオファーする)
            position_encoding: Some(PositionEncodingKind::UTF16),
            // キャラ名にカーソルを合わせた際、そのキャラの設定をMarkdownで表示する。
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            // save 通知(did_save)を有効化するため Kind ではなく Options 形式にする。
            // キャラクター設定ファイル保存時に character_cache を再構築するために使う。
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::INCREMENTAL),
                    save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    ..Default::default()
                },
            )),
            selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
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
    async fn initialized(&self, _params: InitializedParams) {
        debug!("LSP server initialized");

        let req = vec![ConfigurationItem {
            scope_uri: None,
            section: None,
        }];
        let res = self.client.configuration(req).await.unwrap();
        debug!("{:?}", res);

        // (a) ワークスペース初期化時にキャラクターファイルを能動スキャンし character_cache を populate する。
        // LLM補完のtool calling実行を待たずに、人名ハイライトの絞り込みをすぐ使えるようにするため。
        let workspace_paths: Vec<PathBuf> = self.workspace.lock().await.clone();
        for ws in &workspace_paths {
            for path in Self::list_character_files(ws) {
                self.reparse_character_file(&path).await;
            }
        }

        // (c) 外部エディタ/gitなどによるキャラクターファイルの変更も検知できるよう、
        // workspace/didChangeWatchedFiles を動的登録する。クライアントが対応していない場合は
        // register_capability がエラーを返すが、致命的ではないのでログのみで継続する。
        let registration = Registration {
            id: "fifty-four-watch-characters".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers: vec![
                    FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/characters.md".to_string()),
                        kind: None,
                    },
                    FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/characters/*.md".to_string()),
                        kind: None,
                    },
                ],
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            warn!(
                "workspace/didChangeWatchedFiles の動的登録に失敗（クライアント未対応の可能性）: {:?}",
                e
            );
        }

        self.refresh_highlight_names().await;
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    // #[instrument(ret, err)]
    async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
        Ok(())
    }

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

    /// (b) キャラクター設定ファイル保存時に character_cache を再構築する。
    /// 保存直後の内容をディスクから読み直し、人名ハイライトの許可名集合へ即座に反映する。
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.as_str();
        if !self.is_character_file(uri) {
            return;
        }
        debug!("did_save[{}]: character file, reparsing", uri);
        let Some(path) = params.text_document.uri.to_file_path() else {
            warn!("did_save[{}]: failed to convert uri to file path", uri);
            return;
        };
        self.reparse_character_file(&path).await;
        self.refresh_highlight_names().await;
    }

    /// (c) workspace/didChangeWatchedFiles: エディタ外(他プログラム・git等)での
    /// キャラクター設定ファイル変更を検知し character_cache を再構築する。
    /// `initialized` で動的登録した watcher からの通知を受ける。
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            let Some(path) = change.uri.to_file_path().map(|p| p.into_owned()) else {
                warn!(
                    "did_change_watched_files: failed to convert uri to file path: {:?}",
                    change.uri
                );
                continue;
            };
            match change.typ {
                FileChangeType::DELETED => {
                    debug!("did_change_watched_files: removing {:?} from cache", path);
                    self.character_cache.0.lock().remove(&path);
                }
                _ => {
                    debug!("did_change_watched_files: reparsing {:?}", path);
                    self.reparse_character_file(&path).await;
                }
            }
        }
        self.refresh_highlight_names().await;
    }

    // #[instrument]
    async fn did_change(&self, param: DidChangeTextDocumentParams) {
        debug!("did_change");

        self.db.mark_selected_completion(
            param.text_document.uri.as_str(),
            param.content_changes.as_slice(),
        );

        // 全体が送られて来た時
        if param.content_changes.iter().all(|c| c.range.is_none()) {
            debug!(
                "did_change[{}]: full-text sync (no range) -> update_all, record_change SKIPPED",
                param.text_document.uri.as_str()
            );
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

        self.record_change(
            param.text_document.uri.as_str(),
            param.content_changes.as_slice(),
        );

        let _ = self.client.semantic_tokens_refresh().await;
    }

    // #[instrument(ret)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("file closed!");

        self.text.remove(params.text_document.uri.as_str());
        self.update_states.remove(params.text_document.uri.as_str());
    }

    /// ドキュメント全体に対する semantic tokens の問い合わせに応答します。
    ///
    // #[instrument(ret, err)]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<SemanticTokensResult>> {
        debug!("semantic_token_full");

        let uri = params.text_document.uri.as_str();

        let vec = {
            // 共有ストアの行を直接更新する(get_mut)。深さを 0 から畳み込みながら全行を
            // 処理することで、各行の tag / bracket_depth_after キャッシュが書き戻され、
            // 以降の completion がそのまま再利用できる(陳腐化キャッシュもここで修復される)。
            let mut lines = self.text.get_mut(uri).expect("Failed to get text");
            let mut depth = 0u32;
            let mut per_line = Vec::with_capacity(lines.len());
            for line in lines.iter_mut() {
                let (toks, d) = self.highlighter.tokenize_with_depth(line, depth);
                depth = d;
                per_line.push(toks);
            }
            Highlighter::to_semantic_tokens(per_line)
                .into_iter()
                .map(|t| SemanticToken {
                    delta_line: t.delta_line,
                    delta_start: t.delta_start,
                    length: t.length,
                    token_type: t.token_type,
                    token_modifiers_bitset: t.token_modifiers_bitset,
                })
                .collect::<Vec<_>>()
        };

        let tokens = SemanticTokens {
            result_id: None,
            data: vec,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    /// キャラ名にカーソルを合わせた際、そのキャラの設定(全セクション)をMarkdownで表示する。
    /// 対象語が未登録のキャラ名でなければ `Ok(None)` を返し、何も表示しない。
    async fn hover(&self, params: HoverParams) -> tower_lsp_server::jsonrpc::Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        let uri = pos.text_document.uri.as_str();
        let line_no = pos.position.line as usize;
        let utf16_offset = pos.position.character as usize;

        let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
            TryResult::Locked | TryResult::Absent => return Ok(None),
            TryResult::Present(t) => t,
        };
        if line_no >= tmp.len() {
            return Ok(None);
        }

        let highlighter = &self.highlighter;
        let hit =
            crate::cursor_context::token_at(tmp.as_mut_slice(), line_no, utf16_offset, &mut |line| {
                line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
            });
        let Some((_ix, tkn)) = hit else {
            return Ok(None);
        };
        let surface = tmp[line_no].surface(&tkn).to_string();
        drop(tmp);

        let markdown = lookup_character_markdown(&self.character_cache, &surface);

        Ok(markdown.map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    // #[instrument(ret, err)]
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<CompletionResponse>> {
        debug!(
            "completion: partial({:?}), progress({:?})",
            params.partial_result_params.partial_result_token,
            params.work_done_progress_params.work_done_token
        );

        if let Some(context) = params.context {
            match context.trigger_kind {
                CompletionTriggerKind::INVOKED => {
                    // Handle completion triggered by user input
                    debug!("triggered by user input");
                }
                CompletionTriggerKind::TRIGGER_CHARACTER => {
                    // Handle completion triggered by a specific character
                    debug!(
                        "trigger by '{}'",
                        context.trigger_character.unwrap_or("※".to_string())
                    );
                }
                CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS => {
                    // Handle completion triggered for incomplete completion
                    debug!("trigger for incomplete completion");
                }
                _ => {
                    // Handle other trigger kinds
                }
            }
        }

        let uri = params.text_document_position.text_document.uri.as_str();
        let line_no = params.text_document_position.position.line as usize;
        let offset = params.text_document_position.position.character as usize;
        let (context, before, precursor_token) = {
            let before = crate::cursor_context::before_sentences_upto(
                // tmp.as_mut_slice(),
                &self.text,
                uri,
                line_no,
                offset,
                10,
                |ln| {
                    let mut t = match self.text.try_get_mut(uri) {
                        TryResult::Locked => {
                            debug!("{} is locked", uri);
                            return;
                        }
                        TryResult::Absent => {
                            debug!("{} is absent", uri);
                            return;
                        }
                        TryResult::Present(t) => t,
                    };
                    let l = match t.get_mut(ln) {
                        None => {
                            debug!("line {} is absent", ln);
                            return;
                        }
                        Some(l) => l,
                    };
                    l.tokens = self.highlighter.text_to_lindera_token(l.text.as_str());
                },
            );

            if offset > 0 && before.is_empty() {
                return Err(tower_lsp_server::jsonrpc::Error::invalid_params(
                    "offset > 0 && before.is_empty()",
                ));
            }

            // カーソルコンテキスト分類
            let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
                TryResult::Locked => {
                    return Err(tower_lsp_server::jsonrpc::Error::invalid_params(
                        "text for uri is locked",
                    ));
                }
                TryResult::Absent => {
                    return Err(tower_lsp_server::jsonrpc::Error::invalid_params(
                        "No text found for uri",
                    ));
                }
                TryResult::Present(t) => t,
            };

            // カーソル行までの括弧深さを畳み込み、0..=line_no のトークン tag を確定させる。
            // これにより classify_complesion_mode の in_bracket 判定(tag == InBracket)が
            // 行をまたぐ台詞でも正しく機能する。
            self.highlighter
                .ensure_bracket_depth(tmp.as_mut_slice(), line_no);

            let highlighter = &self.highlighter;
            let context = crate::cursor_context::classify_complesion_mode(
                tmp.as_mut_slice(),
                line_no,
                offset,
                // このクロージャはカーソル行より後方の行(next_token フォールバック)専用。
                // 後方行の tag は in_bracket 判定に使われない(in_bracket はカーソル以前の
                // before_tkn のみ参照し、EmptyBracket/BeforeClosingBracket は品詞ベースの
                // is_bracket_close で判定する)ため、深さ未考慮のトークン化で問題ない。
                |line| {
                    line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
                },
            );

            // Zed は補完候補を「カーソル直前の語」で暗黙にフィルタし、候補の
            // label.filter_text()(既定では label 自身)と照合する。filter_text
            // フィールドは label 文字列に含まれる部分でなければ無視される
            // (crates/language_core/src/code_label.rs の filter_range 構築)ため、
            // label に無い任意の文字列を仕込んでフィルタを回避することはできない。
            // 通常の LSP 補完と同じ作法(直前の語トークンを置換し、newText を
            // トークンで始める)に合わせることで、Zed のクエリ(=そのトークン)が
            // 必ず label の接頭辞になり表示される。
            let precursor_token = tmp
                .get(line_no)
                .map(|l| precursor_word(l.text.as_str(), offset).to_string()) // TODO: precursor_tokenが長すぎる
                .unwrap_or_default();

            (context, before, precursor_token)
        };

        let prompt_fn = Backend::ctx_to_prompt_name(context);
        debug!("Prompt: {}", prompt_fn);
        let (prompt, options) =
            crate::frontmatter::load_prompt(prompt_fn).unwrap_or_else(|| panic!("{} not found", prompt_fn));

        let workspace = &self
            .client
            .workspace_folders()
            .await
            .unwrap_or(None)
            .unwrap_or(vec![])
            .first()
            .map(|v| v.uri.to_file_path().unwrap().into_owned())
            .unwrap_or_default();

        let chapter = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let chapter = chapter.file_prefix().unwrap().to_str().unwrap_or("99");
        debug!("Prompt: {:?}", chapter);
        let prompt = prompt.replace("{{CHAPTER}}", chapter);

        // LLM 応答待ちの間、クライアントへ進捗を表示する(Zed の activity indicator)。
        // クライアントがリクエストに付けた workDoneToken があれば優先し、
        // 無ければサーバ発トークンを window/workDoneProgress/create で登録する。
        // Drop ガードで必ずprogressの終了がが送られる
        let progress = CompletionProgress::begin(
            &self.client,
            self.work_done_progress_supported
                .load(std::sync::atomic::Ordering::Relaxed),
            params.work_done_progress_params.work_done_token.clone(),
            "候補を生成しています…",
        )
        .await;

        let mut completion_id = 0u32;
        let raw = self
            .use_llm_with_option(options, async |l| {
                l.add_tool(crate::tools::CharacterInfoTool::new(
                    workspace,
                    self.character_cache.clone(),
                ));
                l.add_tool(crate::tools::PlotInfoTool::new(workspace));
                l.add(Content::Text(prompt));
                l.add(Content::Text(before.join("")));

                l.reasoning_level(0.0); // 速度優先

                completion_id = self.db.record_completion(
                    uri,
                    line_no,
                    offset,
                    l.get_model(),
                    l.build_content().as_str(),
                );
                trace!("Completion {}", completion_id);

                l.chat().await
            })
            .await;

        match raw {
            Ok(response) => {
                debug!("raw Ok.");

                let mut pending: Vec<PendingCandidate> = Vec::new();

                // Zed は補完候補を「カーソル直前の語」で暗黙にフィルタし、
                // 候補の label.filter_text()(既定では label 自身)と照合する。
                // filter_text フィールドは label 文字列に含まれる部分文字列でなければ
                // 無視されるため、label に無い任意の文字列でフィルタを回避することはできない
                // (crates/language_core/src/code_label.rs の filter_range 構築を確認済み)。
                // そこで通常の LSP 補完と同じ作法を取る: 直前の語トークン
                // (precursor_token)を置換対象にし、newText/label をトークンで始める。
                // これで Zed のクエリ(=そのトークン)が必ず label の接頭辞になり表示される。
                // トークンが空(句点・改行・括弧の直後)ならクエリも空になり従来どおり表示される。
                let cursor = Position::new(line_no as u32, offset as u32);
                let precursor_len = crate::types::utf16_len(&precursor_token) as u32;
                let edit_start = Position::new(
                    line_no as u32,
                    offset.saturating_sub(precursor_len as usize) as u32,
                );

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

                        debug!("record candidate");
                        self.db
                            .record_candidate(completion_id, &sr, r, &mut pending);

                        debug!("Completion Item");
                        // newText/label は「直前の語トークン + 続き」。置換 range も
                        // トークン先頭からカーソルまでにすることで、確定時は
                        // トークンがトークン+続きへ置き換わり実質続きの挿入になる。
                        let new_text = format!("{precursor_token}{sr}");
                        let text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                            range: Range::new(edit_start, cursor),
                            new_text: new_text.clone(),
                        }));
                        if new_text.chars().count() > 25 {
                            CompletionItem {
                                label: shorten(&new_text, 25),
                                kind: Some(CompletionItemKind::TEXT),
                                filter_text: Some(precursor_token.clone()),
                                documentation: Some(Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: sr,
                                })),
                                insert_text: Some(new_text.clone()),
                                insert_text_mode: Some(InsertTextMode::AS_IS),
                                text_edit,
                                ..Default::default()
                            }
                        } else {
                            CompletionItem {
                                label: new_text.clone(),
                                kind: Some(CompletionItemKind::TEXT),
                                filter_text: Some(precursor_token.clone()),
                                text_edit,
                                ..Default::default()
                            }
                        }
                    })
                    .collect();

                debug!("set completions");
                self.db.set_completions(uri.to_string(), pending);

                let list = CompletionList {
                    is_incomplete: false,
                    items,
                };

                debug!("Ok completions");
                Ok(Some(CompletionResponse::List(list)))
            }
            Err(err) => {
                error!("Error on completion: {:?}", err);

                if let LlmError::LlmBusy { retry_after: _ } = err {
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            "現在LLMが混雑しています。しばらくしてから再度試してください",
                        )
                        .await;
                }

                Err(tower_lsp_server::jsonrpc::Error::invalid_params(
                    err.to_string(),
                ))
            }
        }
    }

    // #[instrument(ret)]
    async fn did_change_configuration(&self, param: DidChangeConfigurationParams) {
        info!("did_change_configuration: {:?}", param.settings);

        // エディタ側の設定を読む
        let params = vec![ConfigurationItem {
            scope_uri: None,
            section: Some("settings".to_string()),
        }];

        debug!("client.configuration");
        let _ = self.client.configuration(params).await.inspect(|i| {
            i.iter().for_each(|j| {
                debug!("\t{:?}", j);
            });
        });
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

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        debug!("did_change_workspace_folders");
        debug!("\t before {:?}", self.workspace.lock().await);
        for ws in params.event.removed {
            let Some(path) = ws.uri.to_file_path().map(|p| p.into_owned()) else {
                warn!(
                    "did_change_workspace_folders: failed to convert uri to file path: {:?}",
                    ws.uri
                );
                continue;
            };
            let mut w = self.workspace.lock().await;
            if let Some(ix) = w.iter().position(|v| *v == path) {
                w.deref_mut().remove(ix);
            }
        }
        for ws in params.event.added {
            let Some(path) = ws.uri.to_file_path() else {
                warn!(
                    "did_change_workspace_folders: failed to convert uri to file path: {:?}",
                    ws.uri
                );
                continue;
            };
            self.workspace
                .lock()
                .await
                .deref_mut()
                .push(path.into_owned());
        }
        debug!("\t after {:?}", self.workspace.lock().await);
    }
}

impl Backend {
    /// `Backend` を構築する。フィールドを private のまま保つため、
    /// `main()` はこのコンストラクタ経由で `LspService::build` へ渡す。
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            text: DashMap::new(),
            workspace: Arc::new(tokio::sync::Mutex::new(vec![])),
            llm: Arc::new(tokio::sync::Mutex::new(None)),
            background_llm: Arc::new(tokio::sync::Mutex::new(None)),
            character_updater_enabled: std::sync::atomic::AtomicBool::new(true),
            character_updater_min_chars: std::sync::atomic::AtomicUsize::new(
                crate::character_updater::DEFAULT_MIN_CHARS,
            ),
            character_updater_max_chars: std::sync::atomic::AtomicUsize::new(
                crate::character_updater::DEFAULT_MAX_CHARS,
            ),
            character_updater_idle_secs: std::sync::atomic::AtomicU64::new(
                crate::character_updater::DEFAULT_IDLE_SECS,
            ),
            highlighter: Highlighter::new(),
            db: Arc::new(FlightRecorder::open_default()),
            character_cache: CharacterCache::new(),
            update_states: DashMap::new(),
            work_done_progress_supported: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[allow(unused)]
    async fn use_llm<F>(&self, proc: F) -> core::result::Result<String, LlmError>
    where
        F: for<'b, 'a> AsyncFnOnce(
            &'b mut Box<dyn LlmInterface + 'a>,
        ) -> core::result::Result<String, LlmError>,
    {
        let mut ref_llm = self.llm.lock().await;
        if let Some(llm) = ref_llm.deref_mut() {
            debug!("Before use_llm.");
            let ret = proc(llm).await;
            debug!("After use_llm.");
            return ret;
        }

        core::result::Result::Err(LlmError::NotInitialized)
    }

    async fn use_llm_with_option<F>(
        &self,
        option: HashMap<String, String>,
        proc: F,
    ) -> core::result::Result<String, LlmError>
    where
        F: for<'b, 'a> AsyncFnOnce(
            &'b mut Box<dyn LlmInterface + 'a>,
        ) -> core::result::Result<String, LlmError>,
    {
        debug!("Options {:?}", option);
        let mut ref_llm = self.llm.lock().await;

        if let Some(llm) = ref_llm.deref_mut() {
            if let Some(v) = option.get("max_tokens")
                && let Ok(n) = v.parse::<u32>()
            {
                llm.max_tokens(n);
            }
            if let Some(v) = option.get("temperature")
                && let Ok(n) = v.parse::<f64>()
            {
                llm.temperature(n);
            }
            if let Some(v) = option.get("top_p")
                && let Ok(n) = v.parse::<f64>()
            {
                llm.top_p(n);
            }
            if let Some(v) = option.get("stop_sequences") {
                llm.stop_sequences(v.split(',').map(|s| s.to_string()).collect());
            }
            if let Some(v) = option.get("seed")
                && let Ok(n) = v.parse::<u64>()
            {
                llm.seed(n);
            }
            if let Some(v) = option.get("reasoning_effort") {
                if let Ok(level) = v.parse::<f64>() {
                    llm.reasoning_level(level);
                } else if let Ok(eff) = v.parse::<ReasoningEffort>() {
                    llm.reasoning_effort(eff);
                }
            }
            if let Some(v) = option.get("service_tier")
                && let Ok(n) = v.parse::<ServiceTier>()
            {
                llm.service_tier(n);
            }
            if let Some(v) = option.get("verbosity")
                && let Ok(n) = v.parse::<Verbosity>()
            {
                llm.verbosity(n);
            }

            // スキーマが frontmatter に指定されている場合、capability に応じて切り替える
            let has_schema = option.contains_key("schema");
            if let Some(schema_str) = option.get("schema") {
                match serde_json::from_str::<serde_json::Value>(schema_str) {
                    Ok(schema_value) => {
                        let name = option
                            .get("schema_name")
                            .map(|s| s.as_str())
                            .unwrap_or("output");
                        if llm
                            .capabilities()
                            .contains(ModelCapability::STRUCTURED_OUTPUT)
                        {
                            debug!("schema: using JsonSpec (structured output)");
                            llm.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                                name,
                                schema_value,
                            )));
                        } else {
                            debug!("schema: falling back to prompt embedding");
                            llm.add(Content::Text(format!(
                                "回答は次のJSON schemaに厳密にしたがって生成せよ。\n\nJSON Schema:\n{}\n\n最終応答は、スキーマに適合するJSONのみを出力し、JSON以外の文字は一切含めないこと。",
                                schema_str
                            )));
                        }
                    }
                    Err(e) => {
                        return core::result::Result::Err(LlmError::JsonParseError {
                            message: format!("Invalid schema in frontmatter: {}", e),
                        });
                    }
                }
            }

            let result = proc(llm).await;

            // schema が指定されていた場合、レスポンスが valid JSON であることを検証する
            if has_schema {
                return result.and_then(|s| {
                    serde_json::from_str::<serde_json::Value>(&s)
                        .map(|_| s)
                        .map_err(|e| LlmError::JsonParseError {
                            message: format!("LLM response is not valid JSON: {}", e),
                        })
                });
            }
            return result;
        }

        core::result::Result::Err(LlmError::NotInitialized)
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
    /*
       pub fn tokenize_line(&self, url: &str, line_no: usize) {
           debug!("Backend::tokenize_line({:?}, {:?})", url, line_no);
           let tmp = self.text.try_get_mut(url); // ここで2重ロック
           let mut t: RefMut<_, _> = match tmp {
               TryResult::Locked => {
                   error!("text is locked");
                   return;
               }
               TryResult::Absent => {
                   warn!("URL not found in text: {}", url);
                   return;
               }
               TryResult::Present(tmp2) => {
                   if !tmp2[line_no].tokens.is_empty() {
                       return;
                   }
                   tmp2
               }
           };
           // let mut t: &mut Vec<LineData> = tmp2.as_mut();
           let mut l = t
               // .as_mut()
               // .value_mut()   // この辺で参照でなく値になってしまってる疑い
               .get_mut(line_no)
               .unwrap(); //.clone();
           self.highlighter.tokenize(&mut l);
           // debug!(
           //     "\t{:?}",
           //     l
           //         .tokens
           //         .iter()
           //         .take(1),
           // );
       }

       pub fn tokenize_line2(&self, line: &mut LineData) {
           self.highlighter.tokenize(line);
       }
    */
    async fn init_workspace(&self, workspaces: Vec<WorkspaceFolder>) {
        debug!("init_workspace: {:?}", workspaces);
        let mut paths: Vec<PathBuf> = Vec::with_capacity(workspaces.len());
        for ws in &workspaces {
            match ws.uri.to_file_path() {
                Some(p) => paths.push(p.into_owned()),
                None => warn!(
                    "init_workspace: failed to convert uri to file path: {:?}",
                    ws.uri
                ),
            }
        }
        self.workspace.lock().await.append(&mut paths);
    }

    /// URI がキャラクター設定ファイルかどうかを判定する。
    /// characters/*.md または characters.md は更新タスクの対象外とする(自己ループ防止)。
    fn is_character_file(&self, uri: &str) -> bool {
        uri.contains("/characters/") || uri.ends_with("/characters.md")
    }

    /// workspace 配下の全キャラクターファイルパスを列挙する。
    /// `characters.md`（単一ファイル）と `characters/*.md`（ディレクトリ）の両方に対応する
    /// （`find_character_file_path` のファイル探索ロジックを「全件列挙」向けに一般化したもの）。
    fn list_character_files(workspace: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();

        let single = workspace.join("characters").with_extension("md");
        if single.is_file() {
            files.push(single);
        }

        let dir = workspace.join("characters");
        if dir.is_dir()
            && let Ok(entries) = dir.read_dir()
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
                    files.push(path);
                }
            }
        }

        files
    }

    /// 1ファイルを読み込み parse して character_cache に反映する。
    /// 読み込みに失敗した場合はログのみで static に無視する(呼び出し元は複数ファイルを回すため)。
    async fn reparse_character_file(&self, path: &Path) {
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("reparse_character_file: failed to read {:?}: {}", path, e);
                return;
            }
        };

        let characters = parse_all_content(&content);
        self.character_cache.0.lock().insert(
            path.to_path_buf(),
            FileCacheEntry {
                modified,
                characters,
            },
        );
    }

    /// character_cache 全体から人名ハイライトの許可名集合を再構築し Highlighter に反映、
    /// クライアントへ semanticTokens の再取得を要求する。
    /// 名前集合が変化した場合は、キャラ名を Lindera ユーザー辞書に固有名詞として登録し直し、
    /// 旧辞書でトークナイズ済みのキャッシュを破棄する(誤分割されていた名前の再解析のため)。
    async fn refresh_highlight_names(&self) {
        let names = collect_allowed_names(&self.character_cache);
        debug!("refresh_highlight_names: {} 件の許可名", names.len());
        let changed = self.highlighter.set_allowed_names(names);
        if changed {
            if let Err(e) = self.highlighter.rebuild_user_dictionary() {
                warn!("ユーザー辞書の再構築に失敗: {}", e);
            }
            self.invalidate_token_caches();
        }
        if let Err(e) = self.client.semantic_tokens_refresh().await {
            warn!("semantic_tokens_refresh failed: {:?}", e);
        }
    }

    /// 全バッファの Lindera トークンキャッシュを破棄する(次回アクセス時に遅延再トークナイズされる)。
    /// ユーザー辞書の差し替え後、旧辞書による分割結果を捨てるために使う。
    fn invalidate_token_caches(&self) {
        for mut entry in self.text.iter_mut() {
            for line in entry.value_mut().iter_mut() {
                line.tokens.clear();
            }
        }
    }

    /// did_change イベントの情報を更新状態に記録し、発火判定を行う。
    /// 判定は常にこのタイミングのみ(周期タスクは持たない)。
    /// - 現在の変更を取り込む前に直前バーストの idle 判定(`idle_trigger`)を行い、
    ///   idle 確定なら `min_chars` 以上で発火・未満でクリア。
    /// - 現在の変更を取り込んだ後、`max_chars` 以上なら即時発火。
    /// 発火時に spawn する非同期タスクは `crate::character_updater::run` だけ。
    fn record_change(&self, uri: &str, changes: &[TextDocumentContentChangeEvent]) {
        use std::sync::atomic::Ordering::Relaxed;

        if !self.character_updater_enabled.load(Relaxed) {
            debug!("record_change[{}]: disabled, skip", uri);
            return;
        }
        if self.is_character_file(uri) {
            debug!("record_change[{}]: character file, skip", uri);
            return;
        }

        let state_arc = self
            .update_states
            .entry(uri.to_string())
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(UpdateState::default())))
            .clone();

        let now = std::time::Instant::now();
        let delta: usize = changes.iter().map(|c| c.text.chars().count()).sum();
        debug!(
            "record_change[{}]: {} changes, delta={} chars",
            uri,
            changes.len(),
            delta
        );

        // 現在の変更を状態へ取り込む(last_change/first_dirty/accumulated)。
        let apply = |s: &mut UpdateState| {
            s.last_change_at = now;
            if s.first_dirty_at.is_none() {
                s.first_dirty_at = Some(now);
            }
            s.accumulated_chars += delta;
        };

        // spawn 前に Mutex を解放する必要があるため、発火するテキストをここに退避する。
        let mut fire_text: Option<String> = None;

        {
            let mut s = state_arc.lock();

            // 実行中はカウントのみ行い、発火判定はスキップする。
            if s.running {
                apply(&mut s);
                debug!(
                    "record_change[{}]: run in progress, accumulate only (accumulated={})",
                    uri, s.accumulated_chars
                );
                return;
            }

            let idle =
                std::time::Duration::from_secs(self.character_updater_idle_secs.load(Relaxed));
            let min_chars = self.character_updater_min_chars.load(Relaxed);
            let max_chars = self.character_updater_max_chars.load(Relaxed);

            // 1. 直前バーストの idle 判定(現在の変更を取り込む前)。
            let gap = now.duration_since(s.last_change_at);
            let trigger =
                crate::character_updater::idle_trigger(s.accumulated_chars, gap, idle, min_chars);
            debug!(
                "record_change[{}]: idle_trigger -> {:?} (accumulated={}, gap={:?}, idle={:?}, min={}, max={})",
                uri, trigger, s.accumulated_chars, gap, idle, min_chars, max_chars
            );
            match trigger {
                crate::character_updater::Trigger::Fire => {
                    fire_text = Some(crate::character_updater::full_text(&self.text, uri));
                    s.reset();
                    s.running = true;
                    apply(&mut s); // 現在の変更で新バーストを開始
                    debug!(
                        "record_change[{}]: FIRE (idle), new burst started with {} chars",
                        uri, s.accumulated_chars
                    );
                }
                crate::character_updater::Trigger::ClearStale => {
                    s.reset();
                    apply(&mut s);
                    debug!(
                        "record_change[{}]: ClearStale, restart burst with {} chars",
                        uri, s.accumulated_chars
                    );
                }
                crate::character_updater::Trigger::None => {
                    // 2. 打鍵継続中: 現在の変更を取り込み max_chars 即時発火を判定。
                    apply(&mut s);
                    if s.accumulated_chars >= max_chars {
                        fire_text = Some(crate::character_updater::full_text(&self.text, uri));
                        let fired = s.accumulated_chars;
                        s.reset();
                        s.running = true;
                        debug!(
                            "record_change[{}]: FIRE (max_chars reached: {} >= {})",
                            uri, fired, max_chars
                        );
                    } else {
                        debug!(
                            "record_change[{}]: accumulating ({}/{} for max_chars)",
                            uri, s.accumulated_chars, max_chars
                        );
                    }
                }
            }
        }

        if let Some(text) = fire_text {
            debug!(
                "record_change[{}]: spawning crate::character_updater::run ({} chars full text)",
                uri,
                text.chars().count()
            );
            tokio::spawn(crate::character_updater::run(
                uri.to_string(),
                self.workspace.clone(),
                text,
                self.background_llm.clone(),
                self.db.clone(),
                state_arc.clone(),
            ));
        }
    }
}
