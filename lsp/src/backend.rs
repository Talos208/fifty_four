//! LSP ハンドラ本体。
//!
//! `main.rs` から切り出したモジュール。`Backend` はサーバの状態を保持し、
//! `LanguageServer` トレイトの実装として各種 LSP リクエスト/通知に応答する。
//! 構築は `Backend::new` に集約し、フィールドは private のまま保つ
//! (`main()` からは構造体リテラルではなくこのコンストラクタ経由で初期化する)。

use crate::character::CharacterStore;
use crate::character_updater::UpdateState;
use crate::flight_recorder::{FlightRecorder, PendingCandidate};
use crate::highlight::Highlighter;
use crate::llm::{Content, LlmError, LlmInterface};
use crate::progress::CompletionProgress;
use crate::text::{apply_changes, precursor_word, shorten};
use crate::types::{CursorContext, LineData};
use dashmap::DashMap;
use dashmap::mapref::one::RefMut;
use dashmap::try_result::TryResult;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::collections::{HashMap, HashSet};
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tower_lsp_server::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer, UriExt};
use tracing::instrument;

/// Zed が `didOpen` で送ってくる languageId。
/// `extension/languages/fiftyfour/config.toml` の `name = "FiftyFour"` を
/// Zed が小文字化したもの(Zed.log の didOpen 実測値)。`semantic_tokens_provider` の
/// `documentSelector` はこれと一致していないとどのバッファにもマッチしない。
pub(crate) const LANGUAGE_ID: &str = "fiftyfour";

/// `Backend` はサーバの状態を保持する構造体です。
///
/// 現在は `Client` を保持しており、サーバからクライアントへログや通知を送信する際に使用します。
#[derive(Debug)]
pub(crate) struct Backend {
    /// LSP クライアントへのハンドル。メッセージ送信などに使用する。
    client: Client,
    // 文章データ（uri、行ごとのテキスト）(Arc化: plot_sync の非同期ワーカーに clone して渡せるように)
    text: Arc<DashMap<String, Vec<LineData>>>,
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
    // false にすると ACP エージェントのチャット要約を補完プロンプトへ埋め込まない
    chat_context_enabled: std::sync::atomic::AtomicBool,
    chat_context_max_chars: std::sync::atomic::AtomicUsize,

    highlighter: Highlighter,
    // デバッグビルド専用のDB操作
    db: Arc<FlightRecorder>,
    // キャラクター設定ファイルのメモリ上の正本(SSoT)。ディスクはload/dump先でしかない。
    character_store: CharacterStore,
    // URI ごとのキャラクター更新トリガー状態
    update_states: DashMap<String, Arc<parking_lot::Mutex<UpdateState>>>,
    // URI ごとの code_action ジョブ(進行中/完了済みの LLM 呼び出し1件)。
    // 同一選択への後続リクエストがここへ合流する。詳細は `code_action::decide_job` 参照。
    code_action_jobs: DashMap<String, (crate::code_action::JobKey, crate::code_action::RunningJob)>,
    // クライアントが window/workDoneProgress をサポートするか(initialize で判定)
    work_done_progress_supported: std::sync::atomic::AtomicBool,
    // plot.md の URI ごとのリネーム同期状態(章名⇄<章名>.txt)。詳細は `plot_sync` 参照。
    plot_sync: DashMap<String, Arc<parking_lot::Mutex<crate::plot_sync::PlotSyncState>>>,
    plot_sync_enabled: std::sync::atomic::AtomicBool,
    plot_sync_idle_ms: std::sync::atomic::AtomicU64,
    // クライアントが WorkspaceEdit.document_changes 経由の ResourceOp::Rename をサポートするか
    // (initialize で判定。false ならリネームは tokio::fs::rename にフォールバックする)
    client_supports_rename_resource_op: std::sync::atomic::AtomicBool,
}

/// サーバの capabilities を組み立てる。`Backend`(`Client` を保持する)から独立した
/// 自由関数にしてあるのは、テストから `Client` なしで直接呼べるようにするため。
#[instrument(ret)]
fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        hover_provider: Some(HoverProviderCapability::Simple(true)), // キャラ名にカーソルを合わせた際、そのキャラの設定をMarkdownで表示する。
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            // キャラクター設定ファイル保存時に character_store を調和(reconcile)するために使う
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)), // キャラ名(表示名・別名とも)からcharacters.md / characters/*.md の該当キャラ見出しへジャンプする
        implementation_provider: Some(
            // plot.md の `# 章名` 見出しで対応する`<章名>.txt` へジャンプする
            ImplementationProviderCapability::Simple(true),
        ),
        references_provider: Some(OneOf::Left(true)), // キャラ名(表示名・別名とも)の登場箇所をワークスペース直下の本文 `.txt` から横断検索する(Find All References)
        document_symbol_provider: Some(OneOf::Left(true)), // 見出し一覧をアウトライン・パンくずとして提供する
        inlay_hint_provider: Some(OneOf::Left(true)), // plot.md の各 `# 章名` 見出し行末に「現文字数/予定文字数」を表示する
        document_link_provider: Some(DocumentLinkOptions {
            // `.md` 内の `[[ページ名]]` wikilink をクリックジャンプ可能にする
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        semantic_tokens_provider: Some(
            SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                SemanticTokensRegistrationOptions {
                    text_document_registration_options: TextDocumentRegistrationOptions {
                        document_selector: Some(vec![DocumentFilter {
                            language: Some(LANGUAGE_ID.to_string()),
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
                                SemanticTokenType::new("wikiLink"),
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
        // 選択範囲(無ければカーソルの文)を LLM で書き換える code action。
        // 「※」があればそこに当てはまる語、無ければ表現改善の候補を複数提示する。
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::REFACTOR_REWRITE]),
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        })),
        // 「↻ 候補を作り直す」用。code_action が返すキャッシュ済み候補を無視して
        // LLM を呼び直すための command(下記 execute_command 参照)。
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![crate::code_action::REGENERATE_COMMAND.to_string()],
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        }),
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
        // plot.md の章名⇄<章名>.txt のリネーム同期用。`.txt` のリネームを通知してもらう
        // (`did_rename_files` 参照)。フォルダのリネームは対象外(`FileOperationPatternKind::File`)。
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: None,
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                did_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![FileOperationFilter {
                        scheme: Some("file".to_string()),
                        pattern: FileOperationPattern {
                            glob: "**/*.txt".to_string(),
                            matches: Some(FileOperationPatternKind::File),
                            options: None,
                        },
                    }],
                }),
                ..Default::default()
            }),
        }),
        ..ServerCapabilities::default()
    }
}

/// `Backend::refresh_highlight_names` の実体。`character_updater::run` の spawn タスクなど、
/// `&Backend` を持ち込めない `'static` タスクからも共有ハンドルの clone 経由で呼べるよう
/// 自由関数にしてある。character_store の全ワークスペース合計の許可名集合で Lindera
/// ユーザー辞書を再構築し、トークンキャッシュを破棄したうえでクライアントへ
/// semanticTokens の再取得を要求する。
#[instrument(skip(character_store, highlighter, text, client))]
async fn refresh_highlight_names_with(
    character_store: &CharacterStore,
    highlighter: &Highlighter,
    text: &DashMap<String, Vec<LineData>>,
    client: &Client,
) {
    let names = character_store.all_allowed_names();
    debug!(
        "refresh_highlight_names: {} 件の許可名(全ワークスペース合計)",
        names.len()
    );
    if let Err(e) = highlighter.rebuild_user_dictionary(&names) {
        warn!("ユーザー辞書の再構築に失敗: {}", e);
    }
    for mut entry in text.iter_mut() {
        for line in entry.value_mut().iter_mut() {
            line.tokens.clear();
        }
    }
    if let Err(e) = client.semantic_tokens_refresh().await {
        warn!("semantic_tokens_refresh failed: {:?}", e);
    }
}

/// 指定 URI のトークン列を組み立てる。`uri` が未オープン(`text` に無い)なら `None`。
#[instrument(skip(text, highlighter, allowed), ret)]
fn build_semantic_tokens(
    text: &DashMap<String, Vec<LineData>>,
    highlighter: &Highlighter,
    uri: &str,
    is_md: bool,
    allowed: &HashSet<String>,
) -> Option<Vec<SemanticToken>> {
    // 共有ストアの行を直接更新する(get_mut)。深さを 0 から畳み込みながら全行を
    // 処理することで、各行の tag / bracket_depth_after キャッシュが書き戻され、
    // 以降の completion がそのまま再利用できる(陳腐化キャッシュもここで修復される)。
    let mut lines = text.get_mut(uri)?;

    // 見出し行の判定は comrak ベース(`outline::heading_line_levels`、front matter・
    // コードフェンスを正しく除外する)を使う。`.md` 以外(`.txt` 等)では見出しの
    // 概念が無いため計算しない(素の "#" で始まる本文が誤って装飾されるのを防ぐ)。
    let heading_levels: std::collections::HashMap<usize, u8> = if is_md {
        let content = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        crate::outline::heading_line_levels(&content)
            .into_iter()
            .collect()
    } else {
        std::collections::HashMap::new()
    };

    let mut depth = 0u32;
    let mut per_line = Vec::with_capacity(lines.len());
    for (line_no, line) in lines.iter_mut().enumerate() {
        // 深さの畳み込み(tag/bracket_depth_after キャッシュの書き戻し)は見出し行でも
        // 必ず行う。返す通常トークン列だけを見出し行では捨てて装飾用の1トークンに
        // 差し替える。
        let (toks, d) = highlighter.tokenize_with_depth(line, depth, allowed);
        depth = d;

        match heading_levels.get(&line_no) {
            Some(&level) => {
                let length = crate::types::utf16_len(line.text.trim_end()) as u32;
                if length == 0 {
                    per_line.push(Vec::new());
                } else {
                    let token_type = if level <= 1 {
                        crate::highlight::SemanticTokenType::Type as u32
                    } else {
                        crate::highlight::SemanticTokenType::Class as u32
                    };
                    per_line.push(vec![crate::highlight::SemanticToken::new(
                        0, length, token_type, 0,
                    )]);
                }
            }
            None => per_line.push(toks),
        }
    }
    Some(
        Highlighter::to_semantic_tokens(per_line)
            .into_iter()
            .map(|t| SemanticToken {
                delta_line: t.delta_line,
                delta_start: t.delta_start,
                length: t.length,
                token_type: t.token_type,
                token_modifiers_bitset: t.token_modifiers_bitset,
            })
            .collect::<Vec<_>>(),
    )
}

/// `LanguageServer` トレイトの実装。
///
/// ここでは最小限のメソッドのみ実装しており、将来的にホバーや補完などを追加できます。
impl LanguageServer for Backend {
    /// LSP クライアントからの `initialize` リクエストに応答します。
    ///
    /// 返却する `InitializeResult` でサーバの機能（capabilities）をクライアントに伝えます。
    #[instrument(ret, skip(self))]
    async fn initialize(
        &self,
        _param: InitializeParams,
    ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
        // サーバの機能（capabilities）を構成します。
        // ここでは最小限として semanticTokens の提供（空実装）を宣言します。
        debug!("initialize");

        if let Some(ws) = _param.workspace_folders {
            debug!("Workspace: {:?}", ws);
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

        // plot_sync(章名⇄<章名>.txt のリネーム同期)のリネーム実行方法を決める。
        // `document_changes` と `resource_operations` に `Rename` の両方が揃っていなければ
        // `client.apply_edit` の `ResourceOp::Rename` は使えないとみなし、
        // `tokio::fs::rename` へフォールバックする。
        let supports_rename_op = _param
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.workspace_edit.as_ref())
            .is_some_and(|we| {
                we.document_changes == Some(true)
                    && we
                        .resource_operations
                        .as_ref()
                        .is_some_and(|ops| ops.contains(&ResourceOperationKind::Rename))
            });
        self.client_supports_rename_resource_op
            .store(supports_rename_op, std::sync::atomic::Ordering::Relaxed);
        debug!(
            "client supports ResourceOp::Rename via apply_edit: {}",
            supports_rename_op
        );

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

            if let Some(cc) = opt.get("chat_context") {
                debug!("chat_context config found: {:?}", cc);
                if cc.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                    self.chat_context_enabled
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(v) = cc.get("max_chars").and_then(|v| v.as_u64()) {
                    self.chat_context_max_chars
                        .store(v as usize, std::sync::atomic::Ordering::Relaxed);
                }
            } else {
                debug!("no chat_context config; using defaults");
            }
            debug!(
                "chat_context effective: enabled={} max_chars={}",
                self.chat_context_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.chat_context_max_chars
                    .load(std::sync::atomic::Ordering::Relaxed),
            );

            if let Some(ps) = opt.get("plot_sync") {
                debug!("plot_sync config found: {:?}", ps);
                if ps.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                    self.plot_sync_enabled
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(v) = ps.get("idle_ms").and_then(|v| v.as_u64()) {
                    self.plot_sync_idle_ms
                        .store(v, std::sync::atomic::Ordering::Relaxed);
                }
            } else {
                debug!("no plot_sync config; using defaults");
            }
            debug!(
                "plot_sync effective: enabled={} idle_ms={}",
                self.plot_sync_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.plot_sync_idle_ms
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
                    let cl = crate::llm::build_client_async(cfg, "system.md").await;
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
                    let cl = crate::llm::build_client_async(cfg, "system.md").await;
                    debug!(
                        "llm.deferred built.\tmodel: {:?}\n\tservice_target: {}",
                        cl,
                        cl.get_service_target().await
                    );
                    self.background_llm.lock().await.replace(cl);
                }
            }
        }

        let capabilities = server_capabilities();

        Ok(InitializeResult {
            capabilities,
            server_info: None,
        })
    }

    /// `initialized` はクライアントが初期化完了を通知した際に呼ばれます。
    ///
    #[instrument(ret, skip(self))]
    async fn initialized(&self, _params: InitializedParams) {
        debug!("LSP server initialized");

        let req = vec![ConfigurationItem {
            scope_uri: None,
            section: None,
        }];
        match self.client.configuration(req).await {
            Ok(res) => debug!("{:?}", res),
            Err(e) => {
                // 失敗しても致命的ではないので warn出すだけ
                warn!("workspace/configuration の取得に失敗: {:?}", e);
            }
        }

        // (a) ワークスペース初期化時にキャラクターファイルを能動スキャンし character_store へロードする。
        // LLM補完のtool calling実行を待たずに、人名ハイライトの絞り込みをすぐ使えるようにするため。
        let workspace_paths: Vec<PathBuf> = self.workspace.lock().await.clone();
        for ws in &workspace_paths {
            self.character_store.load_workspace(ws).await;
        }

        // (c) 外部エディタ/gitなどによるキャラクターファイルの変更も検知できるよう、workspace/didChangeWatchedFiles を動的登録する。
        let registration = Registration {
            id: "fifty-four-watch-characters".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                // `characters.md` に加え、そこから [[wikilink]] で辿られる任意の .md も
                // 追跡対象になりうるため、.md 全体を監視する。実際に取り込むかどうかは
                // `did_change_watched_files` 側で「characters.md または追跡済み」に絞る
                // (そうしないと plot.md の見出しがキャラクターとして取り込まれてしまう)。
                watchers: vec![FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.md".to_string()),
                    kind: None,
                }],
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            warn!("workspace/didChangeWatchedFiles の動的登録に失敗: {:?}", e);
        }

        self.refresh_highlight_names().await;
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    #[instrument(ret, skip(self))]
    async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
        Ok(())
    }

    #[instrument(ret, skip(self))]
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

        // plot_sync: baseline(ディスク上の .txt 群と一致していると信じる章名の並び)を開いた時点の内容で種付けする。
        if self
            .plot_md_workspace(&params.text_document.uri)
            .await
            .is_some()
        {
            let chapters = crate::plot::parse_plot(&params.text_document.text)
                .chapters
                .into_iter()
                .map(|c| c.name)
                .collect();
            self.plot_sync.insert(
                params.text_document.uri.as_str().to_string(),
                Arc::new(parking_lot::Mutex::new(
                    crate::plot_sync::PlotSyncState::new(chapters),
                )),
            );
        }

        let _ = tokio::join!(
            self.client.semantic_tokens_refresh(),
            self.client.inline_value_refresh(),
            self.client.code_lens_refresh()
        );

        let params = ShowDocumentParams {
            uri: params.text_document.uri,
            external: None,
            take_focus: Some(true),
            selection: None,
        };
        let _ = self.client.show_document(params);
    }

    /// (b) キャラクター設定ファイル保存時、character_store をreconcileする。
    #[instrument(ret, skip(self))]
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.as_str();

        // plot.md の inlay hintは対応する .txt の内容に依存する。plot.md 自身が開いていなければ
        // クライアントは再取得しないので、保存のたびに明示的に再取得を促す
        if uri.ends_with(".txt") {
            let _ = self.client.inlay_hint_refresh().await;
        }

        // plot_sync: 保存は「編集の確定」の明確なシグナルなので、debounce を待たず即座に章名変更を判定・実行する。
        if let Some(ws) = self.plot_md_workspace(&params.text_document.uri).await {
            self.flush_plot_sync(&params.text_document.uri, ws).await;
        }

        if !uri.ends_with(".md") {
            return;
        }
        let Some(path) = params.text_document.uri.to_file_path() else {
            warn!("did_save[{}]: failed to convert uri to file path", uri);
            return;
        };
        let Some(ws) = self.resolve_workspace(&params.text_document.uri).await else {
            warn!("did_save[{}]: 所属ワークスペースが特定できない", uri);
            return;
        };
        // characters.md 自身に加え、wikilink 経由で追跡対象へ昇格したファイルも取り込む
        // (昇格したまま同期されないと、次の自動更新が古い内容で上書きしてしまう)。
        if !self.is_character_file(uri) && !self.character_store.is_tracked(&ws, &path) {
            return;
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("did_save[{}]: failed to read {:?}: {}", uri, path, e);
                return;
            }
        };
        if self.character_store.reconcile(&ws, &path, content) {
            debug!("did_save[{}]: 外部変更として取り込み", uri);
            // wikilink 先の変更を、それを参照する追跡ファイルの included_* へ波及させる。
            self.character_store.refresh_included(&ws);
            self.refresh_highlight_names().await;
        } else {
            debug!("did_save[{}]: 自己書き込みのエコーのため無視", uri);
        }
    }

    /// (c) workspace/didChangeWatchedFiles: エディタ外(他プログラム・git等)でのキャラクター設定ファイル変更を検知し
    /// character_store を調和(reconcile)する。`initialized` で動的登録した watcher からの通知を受ける。
    #[instrument(ret, skip(self))]
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // 変更を取り込んだワークスペース(refresh_included は root 単位で行うため)
        let mut changed_workspaces: HashSet<PathBuf> = HashSet::new();
        // エディタ外での .txt 変更(削除・作成・他プログラムによる書き換え)にも追随
        let mut any_txt_changed = false;
        for change in params.changes {
            let Some(path) = change.uri.to_file_path().map(|p| p.into_owned()) else {
                warn!(
                    "did_change_watched_files: failed to convert uri to file path: {:?}",
                    change.uri
                );
                continue;
            };
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("txt"))
                .unwrap_or(false)
            {
                any_txt_changed = true;
            }
            let Some(ws) = self.resolve_workspace(&change.uri).await else {
                warn!(
                    "did_change_watched_files: 所属ワークスペースが特定できない: {:?}",
                    path
                );
                continue;
            };
            match change.typ {
                FileChangeType::DELETED => {
                    // 追跡していないファイルの削除は無視する(watcher は .md 全体に張っている)。
                    if self.character_store.is_tracked(&ws, &path) {
                        debug!("did_change_watched_files: removing {:?} from store", path);
                        self.character_store.remove(&ws, &path);
                        changed_workspaces.insert(ws.clone());
                    }
                }
                _ => {
                    // watcher は `**/*.md` 全体に張っているため、キャラクター設定として
                    // 取り込むべきファイルだけに絞る(そうしないと plot.md の見出しが
                    // キャラクターとして登録されてしまう)。
                    if !self.is_character_file(change.uri.as_str())
                        && !self.character_store.is_tracked(&ws, &path)
                    {
                        continue;
                    }
                    let content = match tokio::fs::read_to_string(&path).await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!("did_change_watched_files: failed to read {:?}: {}", path, e);
                            continue;
                        }
                    };
                    if self.character_store.reconcile(&ws, &path, content) {
                        debug!(
                            "did_change_watched_files: 外部変更として取り込み: {:?}",
                            path
                        );
                        changed_workspaces.insert(ws.clone());
                    } else {
                        debug!(
                            "did_change_watched_files: 自己書き込みのエコーのため無視: {:?}",
                            path
                        );
                    }
                }
            }
        }
        if !changed_workspaces.is_empty() {
            // wikilink 先の変更を、それを参照する追跡ファイルの included_* へ波及させる。
            for ws in &changed_workspaces {
                self.character_store.refresh_included(ws);
            }
            self.refresh_highlight_names().await;
        }
        if any_txt_changed {
            let _ = self.client.inlay_hint_refresh().await;
        }
    }

    /// `.txt` リネーム → plot.md 見出し書き換え。`workspace.fileOperations.didRename`がクライアントから届く
    /// 順方向(plot.md → txt)自身が起こしたリネームは `pending_self_renames` で無視し、無限ループを防ぐ。
    #[instrument(ret, skip(self))]
    async fn did_rename_files(&self, params: RenameFilesParams) {
        if !self
            .plot_sync_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }

        for f in params.files {
            let Ok(old_uri) = Uri::from_str(&f.old_uri) else {
                continue;
            };
            let Ok(new_uri) = Uri::from_str(&f.new_uri) else {
                continue;
            };
            let Some(old_path) = old_uri.to_file_path().map(|p| p.into_owned()) else {
                continue;
            };
            let Some(new_path) = new_uri.to_file_path().map(|p| p.into_owned()) else {
                continue;
            };

            let is_txt = |p: &PathBuf| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("txt"))
                    .unwrap_or(false)
            };
            if !is_txt(&old_path) || !is_txt(&new_path) {
                continue;
            }

            let Some(ws) = self.resolve_workspace(&new_uri).await else {
                continue;
            };
            // ワークスペース直下のみ対象(サブディレクトリへの移動・元々のパス規約に合わせる)。
            if old_path.parent() != Some(ws.as_path()) || new_path.parent() != Some(ws.as_path()) {
                debug!("did_rename_files: not directly under workspace, skip");
                continue;
            }

            let plot_path = ws.join("plot.md");
            let Some(plot_uri) = Uri::from_file_path(&plot_path) else {
                continue;
            };

            // 自己エコー遮断: 順方向がこのペアを起こしたばかりなら無視する。
            if let Some(state) = self.plot_sync.get(plot_uri.as_str())
                && state.lock().take_self_rename(&old_path, &new_path)
            {
                debug!("did_rename_files: self-echo, ignore");
                continue;
            }

            let (Some(old_name), Some(new_name)) = (
                old_path.file_stem().and_then(|s| s.to_str()),
                new_path.file_stem().and_then(|s| s.to_str()),
            ) else {
                continue;
            };

            self.apply_plot_heading_rename(&plot_uri, &plot_path, old_name, new_name)
                .await;
        }
    }

    #[instrument(ret, skip(self))]
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
            self.note_plot_change(&param.text_document.uri).await;
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

        self.record_change(&param.text_document.uri, param.content_changes.as_slice())
            .await;

        self.note_plot_change(&param.text_document.uri).await;

        let _ = self.client.semantic_tokens_refresh().await;
    }

    #[instrument(ret, skip(self))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("file closed!");

        self.text.remove(params.text_document.uri.as_str());
        self.update_states.remove(params.text_document.uri.as_str());
        self.plot_sync.remove(params.text_document.uri.as_str());
        if let Some((_, (_, job))) = self
            .code_action_jobs
            .remove(params.text_document.uri.as_str())
        {
            // 開いたまま放置された生成中ジョブが self.llm を握り続けないよう止める。
            job.abort.abort();
        }
    }

    async fn semantic_tokens_range(
        &self,
        _params: SemanticTokensRangeParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<SemanticTokensRangeResult>> {
        Ok(None)
    }

    /// ドキュメント全体に対する semantic tokens の問い合わせに応答します。
    #[instrument(ret, skip(self))]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.as_str();
        let allowed = match self.resolve_workspace(&params.text_document.uri).await {
            Some(ws) => self.character_store.allowed_names(&ws),
            None => std::collections::HashSet::new(),
        };

        let is_md = params
            .text_document
            .uri
            .to_file_path()
            .and_then(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("md"))
            })
            .unwrap_or(false);

        let Some(vec) = build_semantic_tokens(&self.text, &self.highlighter, uri, is_md, &allowed)
        else {
            // did_open 前(あるいは既に did_close 済み)の URIは色変えの意味なし
            debug!("semantic_tokens_full[{}]: 未オープンの URI", uri);
            return Ok(None);
        };

        let tokens = SemanticTokens {
            result_id: None,
            data: vec,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    /// キャラ名にカーソルを合わせた際、そのキャラの設定(全セクション)をMarkdownで表示する。
    /// 対象語が未登録のキャラ名でなければ `Ok(None)` を返し、何も表示しない。
    #[instrument(ret, skip(self))]
    async fn hover(&self, params: HoverParams) -> tower_lsp_server::jsonrpc::Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        let uri = pos.text_document.uri.as_str();
        let line_no = pos.position.line as usize;
        let utf16_offset = pos.position.character as usize;

        // カーソル位置のドキュメントが属するワークスペースとマッチしなければhoverを出さない
        // 誤って別ワークスペースのキャラ情報を出さないように
        let Some(ws) = self.resolve_workspace(&pos.text_document.uri).await else {
            return Ok(None);
        };

        let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
            TryResult::Locked | TryResult::Absent => return Ok(None),
            TryResult::Present(t) => t,
        };
        if line_no >= tmp.len() {
            return Ok(None);
        }

        let highlighter = &self.highlighter;
        let hit = crate::cursor_context::token_at(
            tmp.as_mut_slice(),
            line_no,
            utf16_offset,
            &mut |line| {
                line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
            },
        );
        let Some((_ix, tkn)) = hit else {
            return Ok(None);
        };
        let surface = tmp[line_no].surface(&tkn).to_string();
        drop(tmp);

        let allowed = self.character_store.allowed_names(&ws);
        // ハイライトと同じ判定基準(品詞=固有名詞,人名 かつ 許可名一致)を通ったトークンのみ
        // hoverを出す。これによりハイライトされないトークンでhoverだけ表示される食い違いを防ぐ。
        if !Highlighter::is_recognized_name(&tkn.details, &surface, &allowed) {
            return Ok(None);
        }

        let markdown = self.character_store.lookup_markdown(&ws, &surface);

        Ok(markdown.map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    /// キャラ名(表示名・別名とも)にカーソルが合ってたら、該当キャラ見出しへジャンプする。
    /// 対象語が未登録のキャラ名でなければ `Ok(None)` を返す(hoverと同一判定基準)。
    #[instrument(ret, skip(self))]
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params;
        let uri = pos.text_document.uri.as_str();
        let line_no = pos.position.line as usize;
        let utf16_offset = pos.position.character as usize;

        // hoverと同じくワークスペース外へのジャンプを避ける。
        let Some(ws) = self.resolve_workspace(&pos.text_document.uri).await else {
            return Ok(None);
        };

        let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
            TryResult::Locked | TryResult::Absent => return Ok(None),
            TryResult::Present(t) => t,
        };
        if line_no >= tmp.len() {
            return Ok(None);
        }

        let highlighter = &self.highlighter;
        let hit = crate::cursor_context::token_at(
            tmp.as_mut_slice(),
            line_no,
            utf16_offset,
            &mut |line| {
                line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
            },
        );
        let Some((_ix, tkn)) = hit else {
            return Ok(None);
        };
        let surface = tmp[line_no].surface(&tkn).to_string();
        drop(tmp);

        let allowed = self.character_store.allowed_names(&ws);
        // hoverと同一の判定基準(品詞=固有名詞,人名 かつ 許可名一致)を通ったトークンのみジャンプ対象
        if !Highlighter::is_recognized_name(&tkn.details, &surface, &allowed) {
            return Ok(None);
        }

        let locations: Vec<Location> = self
            .character_store
            .lookup_definitions(&ws, &surface)
            .into_iter()
            .filter_map(|(path, range)| {
                let uri = Uri::from_file_path(&path)?;
                Some(Location { uri, range })
            })
            .collect();

        // カーソルが既にキャラ見出し行にある場合、代わりに参照一覧を返す(rust-analyzer / IntelliJ 等と同じ振る舞い)。
        let cur_path: Option<PathBuf> =
            pos.text_document.uri.to_file_path().map(|p| p.into_owned());
        let already_at_definition = cur_path.is_some()
            && locations.iter().any(|loc| {
                loc.range.start.line == line_no as u32
                    // 本文中で呼んだ場合はこの条件に該当しないため、従来通り定義へジャンプする。
                    && loc.uri.to_file_path().map(|p| p.into_owned()) == cur_path
            });
        if already_at_definition {
            let names = self.character_store.lookup_names(&ws, &surface);
            let refs = self.collect_references(&ws, &names).await;
            return Ok(if refs.is_empty() {
                None
            } else {
                Some(GotoDefinitionResponse::Array(refs))
            });
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Array(locations)))
        }
    }

    /// plot.md の `# 章名` 見出し行にカーソルを合わせた際、対応する `<章名>.txt`(本文ファイル)へジャンプする。
    /// ファイルがまだ無ければ空ファイルとして作成してからジャンプする
    /// plot.md 以外、または見出し行以外では `Ok(None)`。
    #[instrument(ret, skip(self))]
    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<GotoImplementationResponse>> {
        let pos = params.text_document_position_params;
        let uri = &pos.text_document.uri;
        let line_no = pos.position.line as usize;

        let is_plot_md = uri
            .to_file_path()
            .and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.eq_ignore_ascii_case("plot.md"))
            })
            .unwrap_or(false);
        if !is_plot_md {
            return Ok(None);
        }

        let Some(lines) = self.text.get(uri.as_str()) else {
            return Ok(None);
        };
        let content = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        drop(lines);

        let Some(ws) = self.resolve_workspace(uri).await else {
            return Ok(None);
        };

        let plot = crate::plot::parse_plot(&content);
        let Some(chapter) = plot.chapters.iter().find(|c| c.heading_line == line_no) else {
            return Ok(None);
        };

        let path = ws.join(format!("{}.txt", chapter.name));
        if tokio::fs::metadata(&path).await.is_err() {
            debug!("goto_implementation: creating {:?}", path);
            if let Err(e) = tokio::fs::write(&path, "").await {
                error!("goto_implementation: failed to create {:?}: {}", path, e);
                return Ok(None);
            }
        }

        let Some(target_uri) = Uri::from_file_path(&path) else {
            return Ok(None);
        };
        Ok(Some(GotoImplementationResponse::Scalar(Location {
            uri: target_uri,
            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        })))
    }

    /// `.md` 内の `[[ページ名]]` wikilink をクリックジャンプ可能にする。記法・解決規則は
    /// `docs/lsp-handlers.md` 参照。リンク先が実在しない場合はリンクとして出さない
    /// (自動作成しない。`goto_implementation` とは対照的な挙動)。
    #[instrument(ret, skip(self))]
    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<Vec<DocumentLink>>> {
        let uri = &params.text_document.uri;
        let Some(lines) = self.text.get(uri.as_str()) else {
            return Ok(None);
        };
        let Some(current_path) = uri.to_file_path() else {
            return Ok(None);
        };
        let is_md = current_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));
        if !is_md {
            return Ok(None);
        }
        let Some(current_dir) = current_path.parent() else {
            return Ok(None);
        };

        let text = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        drop(lines);

        let links: Vec<DocumentLink> = crate::wikilink::find_wikilinks(&text)
            .into_iter()
            .filter_map(|m| {
                let target_path = crate::wikilink::resolve_target(current_dir, &m.target)?;
                let target_uri = Uri::from_file_path(&target_path)?;
                Some(DocumentLink {
                    range: Range::new(
                        Position::new(m.line, m.start_utf16),
                        Position::new(m.line, m.end_utf16),
                    ),
                    target: Some(target_uri),
                    tooltip: None,
                    data: None,
                })
            })
            .collect();

        if links.is_empty() {
            Ok(None)
        } else {
            Ok(Some(links))
        }
    }

    /// キャラ名(表示名・別名とも)にカーソルを合わせて Find All References すると、
    /// ワークスペース直下(非再帰)の本文 `.txt` から登場箇所を横断検索して返す。
    /// 対象語が未登録のキャラ名でなければ `Ok(None)` を返す(hover/goto_definitionと同一判定基準)。
    #[instrument(ret, skip(self))]
    async fn references(
        &self,
        params: ReferenceParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<Vec<Location>>> {
        let pos = params.text_document_position;
        let uri = pos.text_document.uri.as_str();
        let line_no = pos.position.line as usize;
        let utf16_offset = pos.position.character as usize;

        let Some(ws) = self.resolve_workspace(&pos.text_document.uri).await else {
            return Ok(None);
        };

        let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
            TryResult::Locked | TryResult::Absent => return Ok(None),
            TryResult::Present(t) => t,
        };
        if line_no >= tmp.len() {
            return Ok(None);
        }

        let highlighter = &self.highlighter;
        let hit = crate::cursor_context::token_at(
            tmp.as_mut_slice(),
            line_no,
            utf16_offset,
            &mut |line| {
                line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
            },
        );
        let Some((_ix, tkn)) = hit else {
            return Ok(None);
        };
        let surface = tmp[line_no].surface(&tkn).to_string();
        drop(tmp);

        let allowed = self.character_store.allowed_names(&ws);
        if !Highlighter::is_recognized_name(&tkn.details, &surface, &allowed) {
            return Ok(None);
        }

        // 参照検索の対象は .txt のみで定義位置は .md 側にしか無いため、気にしない
        let names = self.character_store.lookup_names(&ws, &surface);
        let locations = self.collect_references(&ws, &names).await;

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    /// `.md`(characters.md / plot.md / memo/*.md)の見出し一覧を階層構造として返す。
    /// タブ上部のパンくずと `editor: toggle outline` のデータ源になる
    /// (docs/plans/document-symbol-outline.md 参照)。
    /// `.txt` には見出し概念が無いため、`.md` 以外は常に `Ok(None)`。
    #[instrument(ret, skip(self))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.as_str();
        if !uri.to_lowercase().ends_with(".md") {
            return Ok(None);
        }

        // inlay_hint と同じく、開いているバッファ(編集中の内容)から全文を復元する
        // ここは選択ジャンプ先を返すだけの読み取り専用処理なので、書き込みロック無しの`get` でよい
        let Some(lines) = self.text.get(uri) else {
            return Ok(None);
        };
        let content = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        drop(lines);

        let symbols = crate::outline::markdown_symbols(&content);
        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Nested(symbols)))
        }
    }

    /// plot.md の各章見出しへ「現文字数/予定文字数」を表示する
    /// 詳細は`docs/lsp-handlers.md` の `inlay_hint` の項参照
    #[instrument(ret, skip(self))]
    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let is_plot_md = uri
            .to_file_path()
            .and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.eq_ignore_ascii_case("plot.md"))
            })
            .unwrap_or(false);
        if !is_plot_md {
            return Ok(None);
        }

        // inlay hint は開いているドキュメントにしか来ないため、バッファから全文を復元する。
        let Some(lines) = self.text.get(uri.as_str()) else {
            return Ok(None);
        };
        let content = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        drop(lines);

        let Some(ws) = self.resolve_workspace(uri).await else {
            return Ok(None);
        };

        let plot = crate::plot::parse_plot(&content);

        // 合計進捗の hint を出すのは、front matter が揃っていて、かつその行がビューポート内にある場合のみ
        let want_total = match (
            plot.front_matter_end_line,
            plot.meta.episodes,
            plot.meta.average_chars,
        ) {
            (Some(fm), Some(_), Some(_)) => Self::line_in_range(fm, &params.range),
            _ => false,
        };

        let open_buffers = self.open_txt_buffers();
        let mut hints = Vec::new();
        let mut total_chars = 0usize;

        for chapter in &plot.chapters {
            let in_range = Self::line_in_range(chapter.heading_line, &params.range);
            if !in_range && !want_total {
                continue;
            }

            let path = ws.join(format!("{}.txt", chapter.name));
            let chars = if let Some(c) = open_buffers.get(&path) {
                crate::plot::count_chars(c)
            } else if let Ok(c) = tokio::fs::read_to_string(&path).await {
                crate::plot::count_chars(&c)
            } else {
                0
            };
            if want_total {
                total_chars += chars;
            }
            if !in_range {
                continue;
            }

            let Some(character) = self.line_utf16_len(uri.as_str(), chapter.heading_line) else {
                continue;
            };
            let label = match plot.meta.average_chars {
                Some(target) => format!("{}/{}", chars, target),
                None => chars.to_string(),
            };
            hints.push(InlayHint {
                position: Position {
                    line: chapter.heading_line as u32,
                    character,
                },
                label: InlayHintLabel::String(label),
                kind: None,
                text_edits: None,
                tooltip: Some(InlayHintTooltip::String(format!("{}.txt", chapter.name))),
                padding_left: Some(true),
                padding_right: None,
                data: None,
            });
        }

        if want_total
            && let Some(fm_line) = plot.front_matter_end_line
            && let (Some(episodes), Some(avg)) = (plot.meta.episodes, plot.meta.average_chars)
            && let Some(character) = self.line_utf16_len(uri.as_str(), fm_line)
        {
            hints.push(InlayHint {
                position: Position {
                    line: fm_line as u32,
                    character,
                },
                label: InlayHintLabel::String(format!("合計 {}/{}", total_chars, episodes * avg)),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            });
        }

        if hints.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hints))
        }
    }

    #[instrument(ret, skip(self))]
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
                |line| {
                    // 後方行の tag は in_bracket 判定に使われなため、深さ未考慮のトークン化で問題ない。
                    line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
                },
            );

            // Zed の補完フィルタ対策で直前の語トークンを算出する。`docs/zed-completion-filtering.md` 参照。
            let precursor_token = tmp
                .get(line_no)
                .map(|l| precursor_word(l.text.as_str(), offset).to_string()) // TODO: precursor_tokenが長すぎる
                .unwrap_or_default();

            (context, before, precursor_token)
        };

        let prompt_fn = Backend::ctx_to_prompt_name(context);
        debug!("Prompt: {}", prompt_fn);
        let (prompt, options) = crate::frontmatter::load_prompt(prompt_fn)
            .unwrap_or_else(|| panic!("{} not found", prompt_fn));

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

        // 本文をプロンプトへ埋め込む(単一パス展開。本文が偶然 "{{CHAPTER}}" 等の
        // プレースホルダ表記を含んでいても再走査で二重展開されない。frontmatter::expand
        // 参照)。テンプレートが {{TEXT}} を持たない(未更新の md)場合のみ、従来どおり
        // 本文を別メッセージとして追加するフォールバックを使う。
        let text_body = before.join("");
        let chat = self.chat_digest(workspace);
        let progress_hint = self
            .progress_hint_for_cursor(uri, line_no, offset, workspace)
            .await;
        let vars = HashMap::from([
            ("CHAPTER", chapter),
            ("TEXT", text_body.as_str()),
            ("CHAT", chat.as_str()),
            ("PROGRESS", progress_hint.as_str()),
        ]);
        let prompt = crate::frontmatter::expand(&prompt, &vars);

        // 直前テキストの末尾文字。候補への句点前置要否の判定に使う
        let prev_tail = text_body.chars().next_back();

        // LLM 応答待ちの間、クライアントへ進捗を表示する。Drop ガードで必ずprogressの終了がが送られる
        // クライアントがリクエストに付けた workDoneToken があれば優先し無ければサーバ発トークンを window/workDoneProgress/create で登録する。
        let _progress = CompletionProgress::begin(
            &self.client,
            self.work_done_progress_supported
                .load(std::sync::atomic::Ordering::Relaxed),
            params.work_done_progress_params.work_done_token.clone(),
            "LLM補完を生成中",
            "候補を生成しています…",
        )
        .await;

        let mut completion_id = 0u32;
        let raw = crate::llm::use_llm_with_option(&self.llm, options, async |l| {
            l.add_tool(crate::tools::CharacterInfoTool::new(
                workspace,
                self.character_store.clone(),
            ));
            l.add_tool(crate::tools::PlotInfoTool::new(workspace));
            l.add(Content::Text(prompt));

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

                // precursor_token(直前の語)を置換対象にし newText/label をその語で
                // 始める(Zed の補完フィルタ対策。`docs/zed-completion-filtering.md` 参照)
                // トークンが空(句点・改行・括弧の直後)なら従来どおりの表示になる
                let cursor = Position::new(line_no as u32, offset as u32);
                let precursor_len = crate::types::utf16_len(&precursor_token) as u32;
                let edit_start = Position::new(
                    line_no as u32,
                    offset.saturating_sub(precursor_len as usize) as u32,
                );

                let items = crate::cursor_context::extract_candidate_lines(&response)
                    .into_iter()
                    .map(|r| {
                        let sr = crate::cursor_context::decorate_candidate(context, r, prev_tail);

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

    /// 選択範囲(無ければカーソルの文)を LLM で書き換える code action。
    ///
    /// - 対象内に「※」があれば、そこに当てはまる語の候補を複数提示する
    /// - 無ければ、意味を変えずに表現を改善した候補を複数提示する
    ///
    /// Zed は `trigger_kind` を送らず選択/カーソル移動のたびにこのハンドラを自動ポーリング
    /// する(`docs/zed-code-action-polling.md` 参照)。そのためゲートは選択範囲の有無のみで
    /// 行い、通ったリクエストは1回目でも即座に LLM を起動する(`code_action::decide_job` で
    /// 同一選択への後続リクエストをジョブへ合流させ、二重起動を防ぐ)。
    #[instrument(ret, skip(self))]
    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<CodeActionResponse>> {
        let has_selection = params.range.start != params.range.end;
        let should_run = match params.context.trigger_kind {
            Some(CodeActionTriggerKind::INVOKED) => true,
            None => has_selection,
            _ => false,
        };
        if !should_run {
            debug!(
                "code_action: skip (trigger_kind={:?}, has_selection={})",
                params.context.trigger_kind, has_selection
            );
            return Err(tower_lsp_server::jsonrpc::Error::new(
                tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
            ));
        }

        let uri = params.text_document.uri.as_str();

        // 対象範囲・モード・対象テキストを1回のロックで確定する(before_sentences_upto は
        // 自前でロックを取るため、その前にこのロックを解放しておく必要がある)。
        let (target_range, mode, target_text) = {
            let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
                TryResult::Locked | TryResult::Absent => return Ok(None),
                TryResult::Present(t) => t,
            };
            let highlighter = &self.highlighter;
            let target_range = if has_selection {
                params.range
            } else {
                crate::cursor_context::sentence_range_at(
                    tmp.as_mut_slice(),
                    params.range.start.line as usize,
                    params.range.start.character as usize,
                    |line| {
                        line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
                    },
                )
            };
            let mode = crate::code_action::decide_mode(tmp.as_slice(), target_range);
            let target_text = crate::code_action::slice_range(tmp.as_slice(), target_range);
            (target_range, mode, target_text)
        };

        if target_text.trim().is_empty() {
            debug!("code_action: target_text is empty, skip");
            return Err(tower_lsp_server::jsonrpc::Error::new(
                tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
            ));
        }

        // TODO: ここでdebounce

        // ジョブ判定: 同一の(選択範囲, 対象テキスト)なら進行中/完了済みのジョブに合流し、
        // 別の選択なら(古いジョブを中断して)新規に LLM を起動する。
        let job_key = crate::code_action::JobKey::new(target_range, &target_text);
        let (decision, stale_abort, join_rx) = {
            let entry = self.code_action_jobs.get(uri);
            let prev_key = entry.as_ref().map(|e| &e.0);
            let decision = crate::code_action::decide_job(prev_key, &job_key);
            let stale_abort = (decision == crate::code_action::Decision::Start)
                .then(|| entry.as_ref().map(|e| e.1.abort.clone()))
                .flatten();
            let join_rx = (decision == crate::code_action::Decision::Join)
                .then(|| entry.as_ref().map(|e| e.1.rx.clone()))
                .flatten();
            // `entry` (DashMap の read guard) はここで drop する。この後に同じ uri へ
            // insert する分岐があり、保持したままだと同一シャードでデッドロックする。
            (decision, stale_abort, join_rx)
        };

        if let Some(abort) = stale_abort {
            // 別の選択へ切り替わった。古い選択の LLM 呼び出しが self.llm を握り続けないよう止める。
            debug!("code_action: aborting stale job (uri={})", uri);
            abort.abort();
        }

        let rx = match decision {
            crate::code_action::Decision::Join => match join_rx {
                Some(rx) => {
                    debug!("code_action: joining running/finished job (uri={})", uri);
                    rx
                }
                None => {
                    // get() から insert() の間に他リクエストがジョブを差し替えた等の競合。
                    // 取りこぼした分は次のリクエストに賭ける(自前で再試行しない)。
                    debug!("code_action: job vanished before join (uri={})", uri);
                    return Ok(None);
                }
            },
            crate::code_action::Decision::Start => {
                debug!("code_action: starting new job (uri={})", uri);
                match self
                    .start_code_action_job(
                        &params.text_document.uri,
                        target_range,
                        mode,
                        &target_text,
                    )
                    .await
                {
                    Some(rx) => rx,
                    None => return Ok(None),
                }
            }
        };

        let _progress = CompletionProgress::begin(
            &self.client,
            self.work_done_progress_supported
                .load(std::sync::atomic::Ordering::Relaxed),
            params.work_done_progress_params.work_done_token.clone(),
            "書き換え候補を生成中",
            "候補を生成しています…",
        )
        .await;

        // task が既に完了していれば即座に返る。進行中ならここで待つ(この await 中に
        // キャンセルされても、待っているのはこのリクエストの future だけで、task 自体は
        // 生き続ける)。
        let mut rx = rx;
        let candidates = match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v
                .clone()
                .expect("wait_for(|v| v.is_some()) guarantees Some"),
            Err(_) => {
                // task が abort/panic して tx が drop された。
                debug!("code_action: job ended without result (uri={})", uri);
                return Ok(None);
            }
        };

        // 結果を配達したら破棄する。「一度計算したら選択が変わるまで無期限にキャッシュ」
        // だと、実際に新規リクエストが届いた場合(選択し直す等)でも古い結果を返し続けて
        // しまう。配達後に破棄しておけば、次に本当に届いたリクエストは必ず新規に LLM を
        // 呼び直す(空/エラーで終わった場合も同様に破棄し、無限に空結果を返し続けない)。
        self.code_action_jobs.remove_if(uri, |_, v| v.0 == job_key);

        if candidates.is_empty() {
            debug!("code_action: no candidates extracted");
            return Ok(None);
        }

        let edit_range = match mode {
            crate::code_action::ActionMode::FillMark { mark } => mark,
            crate::code_action::ActionMode::Rephrase => target_range,
        };

        // 先頭に「↻ 候補を作り直す」を置く。選ぶと command 経由でキャッシュを問答無用で
        // 破棄し、LLM を呼び直して結果を直接適用する(新しい候補一覧をメニューとして
        // 開き直すことは LSP の仕組み上できないため)。
        let retry_action = CodeActionOrCommand::CodeAction(CodeAction {
            title: "↻ 候補を作り直す".to_string(),
            kind: Some(CodeActionKind::REFACTOR_REWRITE),
            command: Some(Command {
                title: "↻ 候補を作り直す".to_string(),
                command: crate::code_action::REGENERATE_COMMAND.to_string(),
                arguments: Some(vec![
                    serde_json::to_value(crate::code_action::RegenerateArgs {
                        uri: params.text_document.uri.clone(),
                        range: target_range,
                    })
                    .expect("RegenerateArgs is always serializable"),
                ]),
            }),
            ..Default::default()
        });

        let mut actions = vec![retry_action];
        actions.extend(candidates.iter().map(|candidate| {
            // 切り詰め不要。CodeActionではtitleが長すぎると自動的に末尾を切りつめ、titleを documentation 相当として表示する
            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    params.text_document.uri.clone(),
                    vec![TextEdit {
                        range: edit_range,
                        new_text: candidate.clone(),
                    }],
                )])),
                ..Default::default()
            };
            CodeActionOrCommand::CodeAction(CodeAction {
                title: candidate.clone(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(edit),
                ..Default::default()
            })
        }));

        Ok(Some(actions))
    }

    /// 「↻ 候補を作り直す」の実装。`code_action` が返すキャッシュ・進行中ジョブを問答無用で
    /// 破棄し、必ず LLM を呼び直す。新しい候補一覧をメニューとして開き直すことは LSP の
    /// 仕組み上できない(`workspace/executeCommand` にはそのための応答経路が無い)ため、
    /// 得られた最初の候補を `workspace/applyEdit` でそのまま適用する。
    #[instrument(ret, skip(self))]
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<LSPAny>> {
        if params.command != crate::code_action::REGENERATE_COMMAND {
            return Ok(None);
        }

        let Some(arg) = params.arguments.into_iter().next() else {
            debug!("execute_command: no arguments for regenerate");
            return Ok(None);
        };
        let args: crate::code_action::RegenerateArgs = match serde_json::from_value(arg) {
            Ok(v) => v,
            Err(err) => {
                error!(
                    "execute_command: failed to parse regenerate args: {:?}",
                    err
                );
                return Ok(None);
            }
        };

        let uri = args.uri.as_str();
        let (mode, target_text) = {
            let tmp = match self.text.try_get(uri) {
                TryResult::Locked | TryResult::Absent => return Ok(None),
                TryResult::Present(t) => t,
            };
            let mode = crate::code_action::decide_mode(tmp.as_slice(), args.range);
            let target_text = crate::code_action::slice_range(tmp.as_slice(), args.range);
            (mode, target_text)
        };
        if target_text.trim().is_empty() {
            return Ok(None);
        }

        // 既存のキャッシュ・進行中ジョブを問答無用で破棄し、必ず LLM を呼び直す。
        if let Some((_, (_, job))) = self.code_action_jobs.remove(uri) {
            job.abort.abort();
        }

        let Some(mut rx) = self
            .start_code_action_job(&args.uri, args.range, mode, &target_text)
            .await
        else {
            return Ok(None);
        };

        let candidates = match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v
                .clone()
                .expect("wait_for(|v| v.is_some()) guarantees Some"),
            Err(_) => {
                debug!("execute_command: job ended without result (uri={})", uri);
                return Ok(None);
            }
        };
        self.code_action_jobs.remove(uri);

        if candidates.is_empty() {
            debug!("execute_command: regenerate produced no candidates");
            return Ok(None);
        }

        let edit_range = match mode {
            crate::code_action::ActionMode::FillMark { mark } => mark,
            crate::code_action::ActionMode::Rephrase => args.range,
        };
        let edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                args.uri.clone(),
                vec![TextEdit {
                    range: edit_range,
                    new_text: candidates[0].clone(),
                }],
            )])),
            ..Default::default()
        };
        if let Err(err) = self.client.apply_edit(edit).await {
            error!("execute_command: apply_edit failed: {:?}", err);
        }

        Ok(None)
    }

    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
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
    #[instrument]
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            text: Arc::new(DashMap::new()),
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
            chat_context_enabled: std::sync::atomic::AtomicBool::new(true),
            chat_context_max_chars: std::sync::atomic::AtomicUsize::new(
                crate::chat_context::DEFAULT_MAX_CHARS,
            ),
            highlighter: Highlighter::new(),
            db: Arc::new(FlightRecorder::open_default()),
            character_store: CharacterStore::new(),
            update_states: DashMap::new(),
            code_action_jobs: DashMap::new(),
            work_done_progress_supported: std::sync::atomic::AtomicBool::new(false),
            plot_sync: DashMap::new(),
            plot_sync_enabled: std::sync::atomic::AtomicBool::new(true),
            plot_sync_idle_ms: std::sync::atomic::AtomicU64::new(
                crate::plot_sync::DEFAULT_PLOT_SYNC_IDLE_MS,
            ),
            client_supports_rename_resource_op: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `code_action` の LLM 呼び出し本体。detached task として起動し、
    /// `code_action_jobs` に `RunningJob` として登録してから受信側を返す
    /// (呼び出し元は `.rx` を待つだけでよい)。プロンプトが見つからなければ `None`。
    /// `code_action` ハンドラの `Decision::Start` と、`execute_command` の
    /// 「↻ 候補を作り直す」の両方から呼ばれる。
    #[instrument(skip(self, target_text), ret)]
    async fn start_code_action_job(
        &self,
        document_uri: &Uri,
        target_range: Range,
        mode: crate::code_action::ActionMode,
        target_text: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<Arc<Vec<String>>>>> {
        let uri = document_uri.as_str();

        let before = crate::cursor_context::before_sentences_upto(
            &self.text,
            uri,
            target_range.start.line as usize,
            target_range.start.character as usize,
            10,
            |ln| {
                let mut t = match self.text.try_get_mut(uri) {
                    TryResult::Locked | TryResult::Absent => return,
                    TryResult::Present(t) => t,
                };
                let Some(l) = t.get_mut(ln) else { return };
                l.tokens = self.highlighter.text_to_lindera_token(l.text.as_str());
            },
        );

        let (prompt_name, mode_str) = match mode {
            crate::code_action::ActionMode::FillMark { .. } => ("prompt_fill_mark.md", "fill_mark"),
            crate::code_action::ActionMode::Rephrase => ("prompt_rephrase.md", "rephrase"),
        };
        let (prompt, options) = match crate::frontmatter::load_prompt(prompt_name) {
            Some(v) => v,
            None => {
                error!("{} not found", prompt_name);
                return None;
            }
        };

        let workspace: PathBuf = self
            .client
            .workspace_folders()
            .await
            .unwrap_or(None)
            .unwrap_or(vec![])
            .first()
            .map(|v| v.uri.to_file_path().unwrap().into_owned())
            .unwrap_or_default();

        let chapter = document_uri.to_file_path().unwrap();
        let chapter = chapter.file_prefix().unwrap().to_str().unwrap_or("99");

        let before_text = before.join("");
        let chat = self.chat_digest(&workspace);
        let progress_hint = self
            .progress_hint_for_cursor(
                uri,
                target_range.start.line as usize,
                target_range.start.character as usize,
                &workspace,
            )
            .await;
        let vars = HashMap::from([
            ("CHAPTER", chapter),
            ("TEXT", before_text.as_str()),
            ("TARGET", target_text),
            ("CHAT", chat.as_str()),
            ("PROGRESS", progress_hint.as_str()),
        ]);
        let prompt = crate::frontmatter::expand(&prompt, &vars);

        // LLM 呼び出し本体は detached task へ切り出す。このリクエストが
        // `$/cancelRequest` で drop されても task は走り続け、後続のリクエストが
        // 結果を拾える。
        let (tx, rx) = tokio::sync::watch::channel(None);
        let llm = self.llm.clone();
        let character_store = self.character_store.clone();
        let client = self.client.clone();
        let db = self.db.clone();
        let uri_owned = uri.to_string();
        let target_text_owned = target_text.to_string();
        let handle = tokio::spawn(async move {
            let mut action_id = -1i64;
            let raw = crate::llm::use_llm_with_option(&llm, options, async |l| {
                l.add_tool(crate::tools::CharacterInfoTool::new(
                    &workspace,
                    character_store,
                ));
                l.add_tool(crate::tools::PlotInfoTool::new(&workspace));
                l.add(Content::Text(prompt));

                l.reasoning_level(0.0); // 速度優先

                action_id = db.record_code_action(
                    &uri_owned,
                    mode_str,
                    &target_text_owned,
                    l.get_model(),
                    l.build_content().as_str(),
                );
                trace!("CodeAction {}", action_id);

                l.chat().await
            })
            .await;

            let candidates = match raw {
                Ok(response) => {
                    db.record_code_action_response(action_id, &response);
                    crate::code_action::parse_candidates(&response)
                }
                Err(err) => {
                    error!("Error on code_action: {:?}", err);
                    if let LlmError::LlmBusy { retry_after: _ } = err {
                        client
                            .show_message(
                                MessageType::WARNING,
                                "現在LLMが混雑しています。しばらくしてから再度試してください",
                            )
                            .await;
                    }
                    Vec::new()
                }
            };
            for (i, candidate) in candidates.iter().enumerate() {
                db.record_code_action_candidate(action_id, i, candidate);
            }
            // 受信側が誰もいなくても(全リクエストがキャンセル済みでも)送信でき、
            // その場合は結果を無視してよい(次に同じ範囲へ来たリクエストが拾う)。
            let _ = tx.send(Some(Arc::new(candidates)));
        });

        self.code_action_jobs.insert(
            uri.to_string(),
            (
                crate::code_action::JobKey::new(target_range, target_text),
                crate::code_action::RunningJob {
                    rx: rx.clone(),
                    abort: handle.abort_handle(),
                },
            ),
        );

        Some(rx)
    }

    #[allow(unused)]
    #[instrument(skip(self, proc))]
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

    /// ACP エージェントが書き出したチャット要約を、プロンプトの `{{CHAT}}` 用に取り出す。
    ///
    /// 「作者がいま何を書こうとしているか」は本文の直前 10 文からは読み取れないため、
    /// Agent Panel での会話を短文生成の手掛かりとして渡す(詳細は [`crate::chat_context`])。
    /// 要約が無い・古い・機能が無効のいずれでも空文字を返す。
    ///
    /// 見出しごと組み立てて返すのは、要約が無いときにテンプレート側へ
    /// 空の見出しだけが残らないようにするため(テンプレートは `{{CHAT}}` を
    /// 1行置くだけでよい)。
    #[instrument(skip(self))]
    fn chat_digest(&self, workspace: &Path) -> String {
        use std::sync::atomic::Ordering::Relaxed;

        if !self.chat_context_enabled.load(Relaxed) {
            return String::new();
        }
        match crate::chat_context::read_digest(workspace, self.chat_context_max_chars.load(Relaxed))
        {
            Some(digest) => format!(
                "# 作者は次のようなことを相談している。続きを考える参考にしてもよい。\n\n{}\n\nこの文面をそのまま候補に含めてはならない。\n",
                digest
            ),
            None => String::new(),
        }
    }

    /// カーソル位置が章の何割地点かを、プロンプトの `{{PROGRESS}}` 用の文として返す。
    ///
    /// 分子はバッファ先頭からカーソル位置までの文字数、分母は `plot.md` の front matter
    /// にある `average_chars`(1話あたりの予定文字数)。`plot.md` が無い・
    /// `average_chars` 未設定・対象バッファが無いなど、進捗を算出できない場合は
    /// 空文字列を返す(`frontmatter::expand` は未知のプレースホルダを `{{NAME}}` の
    /// ままテンプレートに残してしまうため、呼び出し側は必ずこの結果を vars に渡すこと)。
    #[instrument(skip(self))]
    async fn progress_hint_for_cursor(
        &self,
        uri: &str,
        line_no: usize,
        utf16_offset: usize,
        workspace: &Path,
    ) -> String {
        let plot_path = workspace.join("plot.md");
        let Ok(content) = tokio::fs::read_to_string(&plot_path).await else {
            return String::new();
        };
        let plot = crate::plot::parse_plot(&content);
        let Some(average_chars) = plot.meta.average_chars else {
            return String::new();
        };

        let Some(lines) = self.text.get(uri) else {
            return String::new();
        };
        let chars_before_cursor = Self::chars_before_cursor(&lines, line_no, utf16_offset);
        drop(lines);

        crate::plot::progress_hint(chars_before_cursor, average_chars)
    }

    /// バッファ先頭からカーソル位置までの文字数(改行を除く。`plot::count_chars` と同一基準)。
    ///
    /// `cursor_context::before_sentences_upto` の戻り値(直前 N 文だけの窓)とは別物で、
    /// あちらは章の先頭からの累積文字数を表さないため進捗率の分子には使えない。
    #[instrument(skip(lines))]
    fn chars_before_cursor(lines: &[LineData], line_no: usize, utf16_offset: usize) -> usize {
        let mut total: usize = lines
            .iter()
            .take(line_no)
            .map(|l| crate::plot::count_chars(&l.text))
            .sum();
        if let Some(line) = lines.get(line_no) {
            let byte_offset = crate::types::utf16_to_byte_offset(&line.text, utf16_offset);
            total += crate::plot::count_chars(&line.text[..byte_offset]);
        }
        total
    }

    /// 補完用 LLM(`llm.ondemand`)を frontmatter のオプション付きで使う。
    ///
    /// 実体は [`crate::llm::use_llm_with_option`]。ACP エージェントも同じ処理を
    /// 使うため、`Backend` に依存しない自由関数として `llm.rs` に置いてある。
    // #[instrument(skip(self, proc))]
    // async fn use_llm_with_option<F>(
    //     &self,
    //     option: HashMap<String, String>,
    //     proc: F,
    // ) -> core::result::Result<String, LlmError>
    // where
    //     F: for<'b, 'a> AsyncFnOnce(
    //         &'b mut Box<dyn LlmInterface + 'a>,
    //     ) -> core::result::Result<String, LlmError>,
    // {
    //     crate::llm::use_llm_with_option(&self.llm, option, proc).await
    // }

    #[instrument(skip(self))]
    fn update_all(&self, uri: &str, _offset: u32, texts: Vec<String>) {
        self.text.insert(
            uri.to_string(),
            texts
                .iter()
                .map(|t| LineData::from_str(t).unwrap())
                .collect::<Vec<_>>(),
        );
    }

    #[instrument(skip(self))]
    fn update_partial(
        &self,
        uri: &str,
        texts: &[impl AsRef<str> + std::fmt::Debug],
        changes: &[Range],
    ) {
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

    #[instrument]
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

    #[instrument(skip(self))]
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

    /// URI がキャラクター設定ファイル(`characters.md`)かどうかを判定する。
    /// 更新タスクの対象外とする(自己ループ防止)。
    #[instrument(skip(self))]
    fn is_character_file(&self, uri: &str) -> bool {
        uri.ends_with("/characters.md")
    }

    /// URIから、それを含む最長一致のワークスペースrootを解決する。
    /// マッチしない場合は最初のワークスペース(あれば)へフォールバックする。
    #[instrument(skip(self))]
    async fn resolve_workspace(&self, uri: &Uri) -> Option<PathBuf> {
        let doc_path = uri.to_file_path()?.into_owned();
        let roots = self.workspace.lock().await;
        CharacterStore::resolve_workspace_for(&doc_path, &roots)
            .cloned()
            .or_else(|| roots.first().cloned())
    }

    /// `uri` が「ワークスペース直下の `plot.md`」を指しているときだけ、そのワークスペース
    /// root を返す。サブディレクトリの `plot.md` は対象外(plot_sync は破壊的操作なので、
    /// `goto_implementation`/`inlay_hint` の緩い判定より厳格にする)。
    #[instrument(skip(self))]
    async fn plot_md_workspace(&self, uri: &Uri) -> Option<PathBuf> {
        let path = uri.to_file_path()?.into_owned();
        let is_plot_md = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.eq_ignore_ascii_case("plot.md"))
            .unwrap_or(false);
        if !is_plot_md {
            return None;
        }
        let ws = self.resolve_workspace(uri).await?;
        (path.parent() == Some(ws.as_path())).then_some(ws)
    }

    /// `uri`(plot.md)の `plot_sync` 状態を取得する。無ければ空の baseline で新規作成する
    /// (通常は `did_open` が種付けするので、ここでの新規作成は保険)。
    fn plot_sync_state(
        &self,
        uri: &Uri,
    ) -> Arc<parking_lot::Mutex<crate::plot_sync::PlotSyncState>> {
        self.plot_sync
            .entry(uri.as_str().to_string())
            .or_insert_with(|| {
                Arc::new(parking_lot::Mutex::new(
                    crate::plot_sync::PlotSyncState::new(Vec::new()),
                ))
            })
            .clone()
    }

    /// `did_change`(plot.md)から呼ぶ。generation を進めて detached task を spawn するだけで、
    /// 判定・実行はタイマー満了後に行う(`plot_sync::run_after` 参照)。
    #[instrument(skip(self))]
    async fn note_plot_change(&self, uri: &Uri) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.plot_sync_enabled.load(Relaxed) {
            return;
        }
        let Some(ws) = self.plot_md_workspace(uri).await else {
            return;
        };
        let state = self.plot_sync_state(uri);
        let generation = {
            let mut s = state.lock();
            s.generation += 1;
            s.generation
        };
        let idle = std::time::Duration::from_millis(self.plot_sync_idle_ms.load(Relaxed));
        let supports_rename_op = self.client_supports_rename_resource_op.load(Relaxed);

        tokio::spawn(crate::plot_sync::run_after(
            idle,
            generation,
            uri.clone(),
            ws,
            self.text.clone(),
            self.client.clone(),
            state,
            supports_rename_op,
        ));
    }

    /// `did_save`(plot.md)から呼ぶ。debounce を待たず、即座に判定・実行する。
    #[instrument(skip(self))]
    async fn flush_plot_sync(&self, uri: &Uri, ws: PathBuf) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.plot_sync_enabled.load(Relaxed) {
            return;
        }
        let state = self.plot_sync_state(uri);
        let generation = {
            let mut s = state.lock();
            s.generation += 1;
            s.generation
        };
        let supports_rename_op = self.client_supports_rename_resource_op.load(Relaxed);
        crate::plot_sync::run(
            generation,
            uri,
            &ws,
            &self.text,
            &self.client,
            &state,
            supports_rename_op,
        )
        .await;
    }

    /// plot_sync の逆方向本体。plot.md 内の `old_name` 見出しを1件だけ特定し、章名部分だけを
    /// `new_name` に書き換える(装飾・行末コメントを保つため行全体は作り直さない)。
    /// plot.md が開いていれば `apply_edit`、開いていなければサーバーが直接読み書きする。
    #[instrument(skip(self))]
    async fn apply_plot_heading_rename(
        &self,
        plot_uri: &Uri,
        plot_path: &std::path::Path,
        old_name: &str,
        new_name: &str,
    ) {
        let is_open = self.text.contains_key(plot_uri.as_str());
        let content = if let Some(lines) = self.text.get(plot_uri.as_str()) {
            lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            match tokio::fs::read_to_string(plot_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "apply_plot_heading_rename: failed to read {:?}: {}",
                        plot_path, e
                    );
                    return;
                }
            }
        };

        let plot = crate::plot::parse_plot(&content);
        let matches: Vec<_> = plot
            .chapters
            .iter()
            .filter(|c| c.name == old_name)
            .collect();
        if matches.len() != 1 {
            debug!(
                "apply_plot_heading_rename: {} matches for {:?}, skip",
                matches.len(),
                old_name
            );
            return;
        }
        if plot.chapters.iter().any(|c| c.name == new_name) {
            debug!(
                "apply_plot_heading_rename: {:?} already exists as a heading, skip",
                new_name
            );
            return;
        }
        let chapter = matches[0];

        let lines: Vec<&str> = content.lines().collect();
        let Some(line_text) = lines.get(chapter.heading_line) else {
            return;
        };
        let Some((start, end)) = crate::plot::heading_name_span(line_text) else {
            return;
        };

        // 自己編集エコー抑止: apply_edit/直接書き込みの前に baseline を書き換え後の状態へ
        // 進める。これにより誘発された did_change は Noop 判定になり、順方向は発火しない。
        {
            let state = self.plot_sync_state(plot_uri);
            let mut s = state.lock();
            s.generation += 1;
            s.baseline = plot
                .chapters
                .iter()
                .map(|c| {
                    if c.name == old_name {
                        new_name.to_string()
                    } else {
                        c.name.clone()
                    }
                })
                .collect();
        }

        if is_open {
            let start_char = crate::types::utf16_len(&line_text[..start]) as u32;
            let end_char = crate::types::utf16_len(&line_text[..end]) as u32;
            let range = Range::new(
                Position::new(chapter.heading_line as u32, start_char),
                Position::new(chapter.heading_line as u32, end_char),
            );
            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    plot_uri.clone(),
                    vec![TextEdit {
                        range,
                        new_text: new_name.to_string(),
                    }],
                )])),
                ..Default::default()
            };
            if let Err(e) = self.client.apply_edit(edit).await {
                error!("apply_plot_heading_rename: apply_edit failed: {:?}", e);
            }
        } else {
            let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            if let Some(l) = new_lines.get_mut(chapter.heading_line) {
                l.replace_range(start..end, new_name);
            }
            if let Err(e) = tokio::fs::write(plot_path, new_lines.join("\n")).await {
                error!(
                    "apply_plot_heading_rename: failed to write plot.md: {:?}",
                    e
                );
            }
        }

        let _ = self.client.inlay_hint_refresh().await;
    }

    /// 開いている `.txt` バッファを パス→内容 のマップとして集める
    /// (DashMap の走査は同期的に終える。await をまたがせない)。
    ///
    /// 「開いていればバッファ優先(編集中の内容)、無ければディスクから読む」という方針を
    /// `collect_references`(Find All References)と `inlay_hint`(plot.md の文字数表示)が
    /// 共有するため、その前段だけを切り出したもの。
    #[instrument(skip(self))]
    fn open_txt_buffers(&self) -> HashMap<PathBuf, String> {
        let mut open_buffers: HashMap<PathBuf, String> = HashMap::new();
        for entry in self.text.iter() {
            let Ok(uri) = Uri::from_str(entry.key()) else {
                continue;
            };
            let Some(path) = uri.to_file_path().map(|p| p.into_owned()) else {
                continue;
            };
            let is_txt = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("txt"))
                .unwrap_or(false);
            if !is_txt {
                continue;
            }
            let content = entry
                .value()
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            open_buffers.insert(path, content);
        }
        open_buffers
    }

    /// `line_no` が inlay hint リクエストの `range`(ビューポート)に含まれるか。
    /// 行単位の粗い判定で十分(ドキュメント全体を毎回返さないためのフィルタ)。
    // #[instrument]
    fn line_in_range(line_no: usize, range: &Range) -> bool {
        let line_no = line_no as u32;
        line_no >= range.start.line && line_no <= range.end.line
    }

    /// `uri` の `line_no` 行目の UTF-16 長を返す(inlay hint の位置=行末を計算するため)。
    /// バッファが無い・行が無ければ `None`。
    // #[instrument(skip(self))]
    fn line_utf16_len(&self, uri: &str, line_no: usize) -> Option<u32> {
        let lines = self.text.get(uri)?;
        let text = lines.get(line_no)?.text.as_str();
        Some(crate::types::utf16_len(text) as u32)
    }

    /// `names` に含まれる名前の登場箇所を、ワークスペース直下(非再帰)の本文 `.txt` から
    /// 収集する(`references` ハンドラ、および `goto_definition` の定義位置フォールバックの
    /// 共通処理)。開いているバッファがあればその内容(編集中の内容)を優先し、
    /// 無ければディスクから読む。`characters.md` 等の設定・メモ類はスキャンしない
    /// (`references::discover_reference_files` が `.txt` のみを列挙するため)。
    #[instrument(skip(self, names))]
    async fn collect_references(&self, ws: &Path, names: &HashSet<String>) -> Vec<Location> {
        if names.is_empty() {
            return Vec::new();
        }

        let open_buffers = self.open_txt_buffers();

        let highlighter = &self.highlighter;
        let mut hits: Vec<(PathBuf, Range)> = Vec::new();
        for path in crate::references::discover_reference_files(ws) {
            let content = if let Some(c) = open_buffers.get(&path) {
                c.clone()
            } else {
                match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        debug!("collect_references: failed to read {:?}: {}", path, e);
                        continue;
                    }
                }
            };
            let ranges = crate::references::scan_text(&content, names, &mut |line: &str| {
                highlighter.text_to_lindera_token(line)
            });
            for range in ranges {
                hits.push((path.clone(), range));
            }
        }

        hits.sort_by(|(pa, ra), (pb, rb)| {
            pa.cmp(pb)
                .then(ra.start.line.cmp(&rb.start.line))
                .then(ra.start.character.cmp(&rb.start.character))
        });

        hits.into_iter()
            .filter_map(|(path, range)| {
                let uri = Uri::from_file_path(&path)?;
                Some(Location { uri, range })
            })
            .collect()
    }

    /// character_store の全ワークスペース合計の許可名集合で Linderaユーザー辞書を再構築し、
    /// クライアントへ semanticTokens の再取得を要求する。トークナイズ品質の担保だけが
    /// 目的で、どのワークスペースの名前かはここでは区別しない
    /// (ハイライト・hoverの最終判定はワークスペーススコープの許可名集合で別途行う)。
    /// 実体は `refresh_highlight_names_with`(`character_updater::run` の spawn タスク内など、
    /// `&Backend` を持ち込めない箇所とも共有する自由関数)。
    #[instrument(skip(self))]
    async fn refresh_highlight_names(&self) {
        refresh_highlight_names_with(
            &self.character_store,
            &self.highlighter,
            &self.text,
            &self.client,
        )
        .await;
    }

    /// did_change イベントの情報を更新状態に記録し、発火判定を行う。
    /// 判定は常にこのタイミングのみ(周期タスクは持たない)。
    /// - 現在の変更を取り込む前に直前バーストの idle 判定(`idle_trigger`)を行い、
    ///   idle 確定なら `min_chars` 以上で発火・未満でクリア。
    /// - 現在の変更を取り込んだ後、`max_chars` 以上なら即時発火。
    /// 発火時に spawn する非同期タスクは `crate::character_updater::run` だけ。
    /// ワークスペース解決(`resolve_workspace`)を伴うため async。
    #[instrument(skip(self))]
    async fn record_change(&self, doc_uri: &Uri, changes: &[TextDocumentContentChangeEvent]) {
        use std::sync::atomic::Ordering::Relaxed;

        let uri = doc_uri.as_str();

        if !self.character_updater_enabled.load(Relaxed) {
            debug!("record_change[{}]: disabled, skip", uri);
            return;
        }
        if self.is_character_file(uri) {
            debug!("record_change[{}]: character file, skip", uri);
            return;
        }
        let Some(workspace) = self.resolve_workspace(doc_uri).await else {
            debug!(
                "record_change[{}]: ワークスペースが特定できないためスキップ",
                uri
            );
            return;
        };

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
            // character_updater::run の書き込みは character_store.write() 経由でメモリへ
            // 即反映されるが、Lindera ユーザー辞書(全ワークスペース許可名の再構築)は
            // did_save/did_change_watched_files からしか呼ばれておらず、この自動更新サイクル
            // 経由の自己書き込みは reconcile のエコー検出で無視されるため did_save 側からも
            // 発火しない。run 完了後にここで明示的に呼ぶ(&Backendはspawnへ持ち込めないため
            // 共有ハンドルをclone)。
            let character_store = self.character_store.clone();
            let highlighter = self.highlighter.clone();
            let text_store = self.text.clone();
            let client = self.client.clone();
            let background_llm = self.background_llm.clone();
            let db = self.db.clone();
            let uri_owned = uri.to_string();
            let fut = async move {
                crate::character_updater::run(
                    uri_owned,
                    workspace,
                    character_store.clone(),
                    text,
                    background_llm,
                    db,
                    state_arc.clone(),
                )
                .await;
                refresh_highlight_names_with(&character_store, &highlighter, &text_store, &client)
                    .await;
            };
            // tokio::spawn は別タスクとして切り離すため、tracing のスパンコンテキストは
            // 明示的に運ばないと引き継がれない(このままだと character_updater::run の
            // #[instrument] が新しい独立したトレースを作ってしまい、did_change 側の
            // トレースから辿れなくなる)。
            #[cfg(feature = "otel")]
            let fut = {
                use tracing::Instrument;
                fut.instrument(tracing::Span::current())
            };
            tokio::spawn(fut);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LineData {
        text.parse().unwrap()
    }

    #[test]
    fn test_chars_before_cursor_sums_preceding_lines() {
        let lines = vec![line("あいう"), line("えお"), line("かきくけ")];
        // line_no=2 (「かきくけ」の行) のオフセット0 → その前2行分の文字数のみ。
        assert_eq!(Backend::chars_before_cursor(&lines, 2, 0), 3 + 2);
    }

    #[test]
    fn test_chars_before_cursor_counts_partial_current_line_by_utf16() {
        let lines = vec![line("あいうえお")];
        // カーソルがUTF-16オフセット3(「あいう」の直後)にある場合。
        assert_eq!(Backend::chars_before_cursor(&lines, 0, 3), 3);
    }

    #[test]
    fn test_chars_before_cursor_excludes_newlines_like_count_chars() {
        // 各 LineData.text は改行を含まない前提だが、念のため count_chars と
        // 同一基準(改行のみ除外)であることを、行内に改行が無くても壊れないことで確認する。
        let lines = vec![line("あ　い")]; // 全角スペースは数える
        assert_eq!(Backend::chars_before_cursor(&lines, 0, 3), 3);
    }

    #[test]
    fn test_chars_before_cursor_line_no_beyond_buffer_sums_all_lines() {
        let lines = vec![line("あい"), line("うえ")];
        assert_eq!(Backend::chars_before_cursor(&lines, 5, 0), 4);
    }

    /// `extension/languages/fiftyfour/config.toml` の `name = "..."` を素朴に読み取る。
    /// dev-dependency に toml クレートを足したくないための最小限のパース。
    fn extension_language_name() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../extension/languages/fiftyfour/config.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {:?}: {}", path, e));
        let line = content
            .lines()
            .find(|l| l.trim_start().starts_with("name"))
            .unwrap_or_else(|| panic!("no `name = ...` line in {:?}", path));
        line.split('=')
            .nth(1)
            .unwrap()
            .trim()
            .trim_matches('"')
            .to_string()
    }

    /// `server_capabilities()` の semanticTokens.documentSelector が、実際に Zed から
    /// 送られてくる languageId(= extension の `name` を小文字化したもの)と一致しているか
    /// をビルド時に検証する。過去にこれがズレて("fifty_four" vs "fiftyfour")
    /// documentSelector がどのバッファにもマッチしない不発弾になっていたことがある。
    #[test]
    fn test_semantic_tokens_document_selector_matches_extension_language_id() {
        let expected = extension_language_name().to_lowercase();
        assert_eq!(LANGUAGE_ID, expected);

        let caps = server_capabilities();
        let Some(SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(opts)) =
            caps.semantic_tokens_provider
        else {
            panic!("semantic_tokens_provider is not SemanticTokensRegistrationOptions");
        };
        let selectors = opts
            .text_document_registration_options
            .document_selector
            .expect("document_selector must be set");
        assert_eq!(selectors.len(), 1);
        assert_eq!(selectors[0].language.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn test_build_semantic_tokens_returns_none_for_unopened_uri() {
        let text: DashMap<String, Vec<LineData>> = DashMap::new();
        let highlighter = Highlighter::new();
        let allowed = HashSet::new();
        assert!(
            build_semantic_tokens(&text, &highlighter, "file:///not-open.txt", false, &allowed)
                .is_none()
        );
    }

    #[test]
    fn test_build_semantic_tokens_marks_heading_lines() {
        let text: DashMap<String, Vec<LineData>> = DashMap::new();
        text.insert(
            "file:///characters.md".to_string(),
            vec![line("# 見出し"), line("本文行")],
        );
        let highlighter = Highlighter::new();
        let allowed = HashSet::new();
        let tokens =
            build_semantic_tokens(&text, &highlighter, "file:///characters.md", true, &allowed)
                .expect("did_open 済みの URI なので Some のはず");

        // 先頭トークンは見出し行(レベル1)の装飾トークンで、`SemanticTokenType::Type` に
        // なっているはず(`# ` はレベル1 → Type、`## ` 以降は Class)。
        assert_eq!(
            tokens[0].token_type,
            crate::highlight::SemanticTokenType::Type as u32
        );
    }
}
