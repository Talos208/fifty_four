// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, options};
use dashmap::mapref::one::RefMut;
use std::sync::Arc;
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
use crate::llm::{Content, LlmClient, LlmClientBuilder, LlmError, LlmTool};
use std::panic;
use std::path::{Path, PathBuf};
mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}
use dashmap::try_result::TryResult;
use genai::chat::{ReasoningEffort, ServiceTier, Verbosity};
#[allow(unused_imports)]
use indoc::indoc;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use rust_embed::Embed;
use serde_json::{Value, json};
use std::ops::DerefMut;

/// 直近のcompletion候補を記録する構造体（デバッグビルドのみ）
#[derive(Debug, Clone)]
struct PendingCandidate {
    #[cfg(debug_assertions)]
    db_id: i64,
    #[cfg(debug_assertions)]
    candidate: String,
}

/// デバッグビルド専用のDB操作をカプセル化する構造体
#[cfg(debug_assertions)]
#[derive(Debug)]
struct FlightRecorder {
    conn: parking_lot::Mutex<rusqlite::Connection>,
    pending_completions: parking_lot::Mutex<Option<(String, Vec<PendingCandidate>)>>,
}

#[cfg(debug_assertions)]
impl FlightRecorder {
    fn new(path: &PathBuf) -> Self {
        // マイグレーションも済ませてしまう
        let mut c = rusqlite::Connection::open(path).expect("Fail to open database");
        match migrations::migrations::runner().run(&mut c) {
            Ok(_) => {}
            Err(e) => {
                panic!("Fail to migrate: {:?}", e);
            }
        }

        Self {
            conn: parking_lot::Mutex::new(c),
            pending_completions: parking_lot::Mutex::new(None),
        }
    }

    /// INSERT INTO completions ... RETURNING id。失敗時は 0 を返す。
    fn record_completion(
        &self,
        uri: &str,
        line_no: usize,
        offset: usize,
        model: &str,
        prompt: &str,
    ) -> u32 {
        use std::time::Duration;

        if let Some(db) = self.conn.try_lock_for(Duration::from_secs(1)) {
            db.query_row(
                indoc!(
                    "INSERT INTO completions
                    (document_uri, cursor_line, cursor_character, model_name, prompt)
                    VALUES (?,?,?,?,?) RETURNING id;"
                ),
                rusqlite::params![
                    uri,
                    line_no.to_string().as_str(),
                    offset.to_string().as_str(),
                    model,
                    prompt,
                ],
                |row| row.get(0),
            )
            .unwrap_or(0)
        } else {
            0
        }
    }

    /// INSERT INTO completion_candidates ... RETURNING id。成功時に pending に push。
    fn record_candidate(
        &self,
        completion_id: u32,
        candidate_text: &str,
        display_text: &str,
        pending: &mut Vec<PendingCandidate>,
    ) {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            match db.query_row(
                indoc!(
                    "INSERT INTO completion_candidates
                    (completion_id, rank, candidate)
                    VALUES (?,?,?) RETURNING id;"
                ),
                rusqlite::params![completion_id, 0, candidate_text],
                |row| row.get::<_, i64>(0),
            ) {
                Ok(id) => pending.push(PendingCandidate {
                    db_id: id,
                    candidate: display_text.to_string(),
                }),
                Err(err) => debug!("Failed to insert completion_candidate: {}", err),
            }
        }
    }

    fn set_completions(&self, uri: String, candidates: Vec<PendingCandidate>) {
        use std::time::Duration;

        if let Some(mut cmp) = self
            .pending_completions
            .try_lock_for(Duration::from_secs(1))
        {
            *cmp = Some((uri, candidates));
        }
    }

    fn mark_selected_completion(
        &self,
        uri: &str,
        content_changes: &[TextDocumentContentChangeEvent],
    ) {
        let (pending_uri, candidates) = {
            let Some(cmp) = self
                .pending_completions
                .try_lock_for(std::time::Duration::from_secs(1))
            else {
                return;
            };

            cmp.clone().unwrap_or(("".to_string(), vec![]))
        };

        if pending_uri != uri {
            return;
        }

        for change in content_changes {
            if let Some(c) = candidates.iter().find(|c| c.candidate == change.text) {
                use std::time::Duration;

                let Some(db) = self.conn.try_lock_for(Duration::from_secs(1)) else {
                    return;
                };
                if let Err(e) = db.execute(
                    "UPDATE completion_candidates SET selected = true WHERE id = ?;",
                    rusqlite::params![c.db_id],
                ) {
                    debug!("Failed to update completion_candidates: {}", e);
                }

                break;
            }
        }
    }
}

#[cfg(not(debug_assertions))]
#[derive(Debug)]
struct FlightRecorder {}

#[cfg(not(debug_assertions))]
impl FlightRecorder {
    fn new(_path: &PathBuf) -> Self {
        Self {}
    }

    fn record_completion(
        &self,
        _uri: &str,
        _line_no: usize,
        _offset: usize,
        _model: &str,
        _prompt: &str,
    ) -> u32 {
        0u32
    }

    fn record_candidate(
        &self,
        _completion_id: u32,
        _candidate_text: &str,
        _display_text: &str,
        _pending: &mut Vec<PendingCandidate>,
    ) {
    }

    fn set_completions(&self, _uri: String, _candidates: Vec<PendingCandidate>) {}

    fn mark_selected_completion(
        &self,
        _uri: &str,
        _content_changes: &[TextDocumentContentChangeEvent],
    ) {
    }
}

#[derive(Debug, PartialEq, Clone)]
enum CharacterAttribute {
    Appearance,
    Background,
    Expression,
    Personality,
    Relationship,
    Role,
    Style,
    Weakness,
}

impl TryFrom<&str> for CharacterAttribute {
    type Error = String;
    fn try_from(s: &str) -> std::result::Result<Self, Self::Error> {
        match s {
            "appearance" | "容姿" | "特徴" | "外見" | "体格" | "風貌" | "風体" | "顔立ち"
            | "印象" | "身体的特徴" => Ok(Self::Appearance),
            "background" | "出自" | "出身" | "生い立ち" | "家庭環境" | "ルーツ" | "血筋"
            | "背景" | "経歴" | "来歴" | "過去" | "前歴" | "履歴" => {
                Ok(Self::Background)
            }
            "expression" | "口調" | "話し方" | "語調" | "言葉遣い" | "一人称" | "台詞" | "癖"
            | "仕草" | "習慣" | "ルーティン" | "口癖" => Ok(Self::Expression),
            "personality" | "性格" | "気質" | "人柄" | "気性" | "内面" | "人間性" | "価値観"
            | "信条" | "信念" | "哲学" | "美学" | "動機" => Ok(Self::Personality),
            "relationship" | "関係" | "交友" | "因縁" | "絆" | "家族" => {
                Ok(Self::Relationship)
            }
            "role" | "立場" | "地位" | "身分" | "階級" | "役職" | "肩書" | "職務" | "役割"
            | "任務" | "所属" => Ok(Self::Role),
            "style" | "描写" | "文体" | "視点" | "表現" => Ok(Self::Style),
            "weakness" | "弱点" | "急所" | "脆さ" | "欠点" | "短所" | "難点" | "問題点" => {
                Ok(Self::Weakness)
            }
            _ => Err(format!("No such attribute {}", s)),
        }
    }
}

/// タグ付きコンテンツ。1つの見出しセクション（属性）に対応する。
#[derive(Debug, Clone)]
struct TaggedContent {
    /// 見出しを「・」で分割したタグ群
    tags: Vec<CharacterAttribute>,
    /// セクションのプレーンテキスト
    text: String,
}

/// 1キャラクター分のキャッシュ
#[derive(Debug, Clone)]
struct CharacterEntry {
    sections: Vec<TaggedContent>,
}

/// 1ファイル分のキャッシュ
#[derive(Debug)]
struct FileCacheEntry {
    modified: std::time::SystemTime,
    /// key: heading 全文（部分一致検索用）
    characters: HashMap<String, CharacterEntry>,
}

/// 全ファイルのキャッシュ (Arc で複数の CharacterInfoTool インスタンス間で共有)
#[derive(Debug, Clone)]
struct CharacterCache(Arc<parking_lot::Mutex<HashMap<PathBuf, FileCacheEntry>>>);

impl CharacterCache {
    fn new() -> Self {
        Self(Arc::new(parking_lot::Mutex::new(HashMap::new())))
    }
}

#[derive(Debug)]
struct CharacterInfoTool {
    workspace: PathBuf,
    cache: CharacterCache,
}

#[async_trait]
impl LlmTool for CharacterInfoTool {
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "character_name": {
                    "type": "string",
                    "description": "設定を取得したいキャラクターの名前"
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "enum": ["role", "appearance", "personality", "expression", "background", "relationship", "weakness", "style"],
                    },
                    "uniqueItems": true,
                    "minItems": 1,
                    "description": "取得したい属性のタグ"
                }
            },
            "required": ["character_name", "tags"],
        })
    }

    fn name(&self) -> &str {
        "character_info"
    }

    fn description(&self) -> &str {
        "キャラクターの設定を取得する"
    }

    async fn invoke(
        &self,
        _args: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, LlmError> {
        let name = _args["character_name"].as_str().unwrap_or("");
        let tags: Vec<String> = _args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        debug!("CharacterInfoTool({}, {:?})", name, tags);

        let path = self.find_character_file_path(name)?;

        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        // キャッシュ確認（ロックは HashMap lookup の間だけ保持）
        {
            let cache = self.cache.0.lock();
            if let Some(entry) = cache.get(&path)
                && entry.modified == modified
            {
                let result = Self::search_cache(entry, name, &tags);
                debug!("\t{:?}", result.map(|r| shorten_middle(&r, 40)))
            }
        }

        // キャッシュミス: ファイルを読んでパース
        let content =
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| LlmError::GenericError {
                    message: format!("Failed to read {:?}: {}", &path, e),
                })?;
        debug!("{}", shorten_middle(&content, 40));

        let characters = parse_all_content(&content);
        let file_entry = FileCacheEntry {
            modified,
            characters,
        };

        let result = Self::search_cache(&file_entry, name, &tags);
        self.cache.0.lock().insert(path, file_entry);
        debug!("\t{:?}", result.as_ref().map(|r| shorten_middle(r, 40)));
        result
    }
}

impl CharacterInfoTool {
    #[deny(clippy::new_ret_no_self)]
    fn new(workspace: &Path, cache: CharacterCache) -> Box<dyn LlmTool> {
        Box::new(Self {
            workspace: workspace.to_path_buf(),
            cache,
        })
    }

    fn find_character_file_path(&self, name: &str) -> std::result::Result<PathBuf, LlmError> {
        let single = self.workspace.join("characters").with_extension("md");
        trace!("{:?}", &single);
        if single.exists() && single.is_file() {
            return Ok(single);
        }

        let dir = self.workspace.join("characters");
        if !dir.exists() || !dir.is_dir() {
            return Err(LlmError::GenericError {
                message: format!("Found no directory {:?}", &dir),
            });
        }

        for entry in dir
            .read_dir()
            .map_err(|_| LlmError::GenericError {
                message: format!("Failed to read directory {:?}", &dir),
            })?
            .flatten()
        {
            debug!("{:?}", entry.file_name());
            if entry.file_name().to_string_lossy().starts_with(name) {
                return Ok(entry.path());
            }
        }

        Err(LlmError::GenericError {
            message: format!("Found no file for '{}' in {:?}", name, &dir),
        })
    }

    fn search_cache(
        entry: &FileCacheEntry,
        name: &str,
        tags: &[String],
    ) -> std::result::Result<String, LlmError> {
        // キャラクター名は部分一致で検索
        let Some((_, char_entry)) = entry.characters.iter().find(|(k, _)| k.contains(name)) else {
            return Err(LlmError::GenericError {
                message: format!("Character '{}' not found", name),
            });
        };

        let tag_attrs = tags
            .iter()
            .filter_map(|t| CharacterAttribute::try_from(t.as_str()).ok())
            .collect::<Vec<_>>();

        let matched = char_entry
            .sections
            .iter()
            .filter(|s| tag_attrs.iter().any(|t| s.tags.contains(t)))
            .map(|s| s.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>();

        if matched.is_empty() {
            Err(LlmError::GenericError {
                message: format!("No sections matching tags {:?} for '{}'", tags, name),
            })
        } else {
            Ok(matched.join("\n\n"))
        }
    }
}

/// ファイル内の heading 構造からキャラクターを表す heading レベルを推定する。
///
/// 各 heading レベルの出現回数と「直後に level+1 の heading が続くか」を調べ、
/// 最も出現回数が多い「子持ちレベル」を返す。タイ時は低レベル優先。
fn detect_char_level<'a>(root: &'a AstNode<'a>) -> u8 {
    let mut counts: HashMap<u8, usize> = HashMap::new();
    let mut has_sub: Vec<u8> = Vec::new();
    let mut prev: u8 = 0;

    for node in root.children() {
        if let NodeValue::Heading(h) = node.data.borrow().value {
            *counts.entry(h.level).or_default() += 1;
            if prev > 0 && h.level > prev && !has_sub.contains(&prev) {
                has_sub.push(prev);
            }
            prev = h.level;
        }
    }

    counts
        .iter()
        .filter(|(l, _)| has_sub.contains(l))
        .max_by(|(l1, c1), (l2, c2)| c1.cmp(c2).then(l2.cmp(l1)))
        .map(|(l, _)| *l)
        .unwrap_or(0)
}

/// Markdown 文字列をパースし、全キャラクターの全セクションを `HashMap` で返す。
///
/// キーは heading 全文（例: "ジェフ・クライン（艦長）"）。
/// 属性 heading のテキストを「・」で分割してタグ群を生成する。
fn parse_all_content(content: &str) -> HashMap<String, CharacterEntry> {
    let arena = Arena::new();
    let mut options = comrak::Options::default();
    options.extension = comrak::options::Extension::builder()
        .cjk_friendly_emphasis(true)
        .greentext(true)
        .multiline_block_quotes(true)
        .table(true)
        .tasklist(true)
        .wikilinks_title_before_pipe(true)
        .build();
    options.parse = options::Parse::builder()
        .relaxed_tasklist_matching(true)
        .smart(true)
        .tasklist_in_table(true)
        .build();
    options.render = options::Render::builder()
        .gfm_quirks(true)
        .ignore_empty_links(true)
        .build();

    let root = comrak::parse_document(&arena, content, &options);
    let char_level = detect_char_level(root);
    if char_level == 0 {
        return HashMap::new();
    }

    let mut characters: HashMap<String, CharacterEntry> = HashMap::new();
    let mut current_char: Option<String> = None;
    let mut current_section: Option<TaggedContent> = None;

    // 現在のセクションをキャラクターエントリに flush するクロージャ相当のマクロ
    macro_rules! flush_section {
        () => {
            if let (Some(ref char_name), Some(section)) =
                (current_char.as_ref(), current_section.take())
            {
                if !section.text.trim().is_empty() {
                    characters
                        .entry(char_name.to_string())
                        .or_insert_with(|| CharacterEntry {
                            sections: Vec::new(),
                        })
                        .sections
                        .push(section);
                }
            }
        };
    }

    for node in root.children() {
        let val = node.data.borrow().value.clone();
        match val {
            NodeValue::Heading(h) if h.level <= char_level => {
                flush_section!();
                current_section = None;
                if h.level == char_level {
                    let t = heading_text(node);
                    trace!("{}", t);
                    current_char = Some(t);
                } else {
                    // タイトルなどキャラクターレベルより上の heading はスキップ
                    current_char = None;
                }
            }
            NodeValue::Heading(h) if h.level == char_level + 1 && current_char.is_some() => {
                flush_section!();
                let t = heading_text(node);
                trace!("{}", t);
                let tags = t
                    .split(['・', '、', ',', '/', ' '])
                    .filter_map(|s| CharacterAttribute::try_from(s).ok())
                    .collect::<Vec<_>>();
                current_section = Some(TaggedContent {
                    tags,
                    text: String::new(),
                });
            }
            _ => {
                // コンテンツノードおよびそれより深い heading はテキストとして追記
                if let Some(ref mut section) = current_section {
                    let text = node_to_plain_text(node);
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if !section.text.is_empty() {
                            section.text.push('\n');
                        }
                        section.text.push_str(trimmed);
                    }
                }
            }
        }
    }

    flush_section!();
    characters
}

/// 見出しノードの直接子から `Text` ノードを結合してキャラクター名や属性名を返す。
fn heading_text<'a>(node: &'a AstNode<'a>) -> String {
    node.children()
        .filter_map(|c| {
            if let NodeValue::Text(ref cow) = c.data.borrow().value {
                Some(cow.as_ref().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// ブロックノードを深さ優先で走査してプレーンテキストを返す。
fn node_to_plain_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut result = String::new();
    for edge in node.traverse() {
        match edge {
            NodeEdge::Start(n) => match &n.data.borrow().value {
                NodeValue::Text(cow) => result.push_str(cow.as_ref()),
                NodeValue::SoftBreak | NodeValue::LineBreak => result.push('\n'),
                _ => {}
            },
            NodeEdge::End(n) => {
                if let NodeValue::Paragraph = n.data.borrow().value {
                    result.push('\n');
                }
            }
        }
    }
    result
}

pub fn shorten(s: &str, len: usize) -> String {
    if s.chars().count() > len {
        s.chars().take(len - 2).collect::<String>() + "……"
    } else {
        s.to_owned()
    }
}

pub fn shorten_middle(s: &str, len: usize) -> String {
    let c = &s.chars();
    let l = c.clone().count();
    if l > 25 && l > len {
        c.clone()
            .take(len - 12)
            .chain("……".chars())
            .chain(c.clone().skip(l - 10))
            .collect::<String>()
    } else {
        s.to_owned()
    }
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
    //ワークスペース
    #[allow(unused)]
    workspace: tokio::sync::Mutex<Vec<WorkspaceFolder>>,
    // LLMクライアントへのハンドル
    llm: tokio::sync::Mutex<Option<Box<dyn LlmClient>>>,

    highlighter: Highlighter,
    // デバッグビルド専用のDB操作
    db: FlightRecorder,
    // キャラクター設定ファイルのパース結果キャッシュ
    character_cache: CharacterCache,
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
    async fn initialize(
        &self,
        _param: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
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

        if let Some(opt) = _param.initialization_options
            && let Some(llm) = opt.get("llm")
        {
            // LLMクライアントを初期化
            let mut builder = LlmClientBuilder::from_value(llm).sys_prompt(
                Asset::get("system.md")
                    .map(|d| String::from_utf8_lossy(d.data.as_ref()).to_string()),
            );

            llm.get("model").inspect(|v| {
                builder.model(v.as_str().unwrap());
            });

            llm.get("url").inspect(|v| {
                builder.url(v.as_str().unwrap());
            });

            // builder.add_tool(CharacterInfoTool::new());

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
    }

    /// サーバのシャットダウン要求を処理します。
    ///
    /// 現在は特別なクリーンアップを行わず、即座に成功を返します。
    // #[instrument(ret, err)]
    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
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

    // #[instrument]
    async fn did_change(&self, param: DidChangeTextDocumentParams) {
        debug!("did_change");

        self.db.mark_selected_completion(
            param.text_document.uri.as_str(),
            param.content_changes.as_slice(),
        );

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

    /// ドキュメント全体に対する semantic tokens の問い合わせに応答します。
    ///
    // #[instrument(ret, err)]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> tower_lsp::jsonrpc::Result<Option<SemanticTokensResult>> {
        debug!("semantic_token_full");

        let uri = params.text_document.uri.as_ref();

        self.highlighter.initialize();

        let vec = {
            let mut lines = self.text.get(uri).expect("Failed to get text").to_vec();
            let tokens = lines
                .iter_mut()
                .map(|l| self.highlighter.tokenize(l))
                .collect::<Vec<_>>();
            Highlighter::to_semantic_tokens(tokens)
        };

        let tokens = SemanticTokens {
            result_id: None,
            data: vec,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    // #[instrument(ret, err)]
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CompletionResponse>> {
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
        let (context, before) = {
            self.highlighter.initialize(); // TODO 正しいdepthを割り当てたい

            let before = cursor_context::before_sentences_upto(
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
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "offset > 0 && before.is_empty()",
                ));
            }

            // カーソルコンテキスト分類
            let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
                TryResult::Locked => {
                    return Err(tower_lsp::jsonrpc::Error::invalid_params(
                        "text for uri is locked",
                    ));
                }
                TryResult::Absent => {
                    return Err(tower_lsp::jsonrpc::Error::invalid_params(
                        "No text found for uri",
                    ));
                }
                TryResult::Present(t) => t,
            };

            let context = cursor_context::classify_complesion_mode(
                tmp.as_mut_slice(),
                line_no,
                offset,
                |ln| {
                    let mut t = self.text.get_mut(uri).unwrap();
                    let l = t.get_mut(ln).unwrap();
                    l.tokens = self.highlighter.text_to_lindera_token(l.text.as_str());
                },
            );
            (context, before)
        };

        let prompt_fn = Backend::ctx_to_prompt_name(context);
        debug!("Prompt: {}", prompt_fn);
        let mut prompt = String::from_utf8_lossy(
            Asset::get(prompt_fn)
                .unwrap_or_else(|| panic!("{} not found", prompt_fn))
                .data
                .as_ref(),
        )
        .to_string();

        // front matter処理
        let options = if let Some(ext) = Path::new(prompt_fn).extension()
            && ext.to_string_lossy() == "md"
        {
            debug!("Front matter...");
            let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
            let parsed_matter = matter.parse::<HashMap<String, String>>(prompt.as_str());
            parsed_matter
                .map(|v| {
                    debug!("\tparsed {}", v.excerpt.unwrap_or_default());
                    prompt = v.content;
                    v.data
                })
                .unwrap_or({
                    debug!("\tNo front matter.");
                    None
                })
        } else {
            debug!("No ext. on file");
            None
        }
        .unwrap_or(HashMap::new());

        let mut completion_id = 0u32;
        let raw = self
            .use_llm_with_option(options, async |l| {
                l.add_tool(CharacterInfoTool::new(
                    &self
                        .client
                        .workspace_folders()
                        .await
                        .unwrap_or(None)
                        .unwrap_or(vec![])
                        .first()
                        .map(|v| v.uri.to_file_path().unwrap())
                        .unwrap_or_default(),
                    self.character_cache.clone(),
                ));
                l.add(Content::Text(prompt));
                l.add(Content::Text(before.join("")));

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
                        if sr.chars().count() > 25 {
                            CompletionItem {
                                label: shorten(&sr, 25),
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

                Err(tower_lsp::jsonrpc::Error::invalid_params(err.to_string()))
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
            let mut w = self.workspace.lock().await;
            if let Some(ix) = w.iter().position(|v| v.uri == ws.uri) {
                w.deref_mut().remove(ix);
            }
        }
        for ws in params.event.added {
            self.workspace.lock().await.deref_mut().push(ws);
        }
        debug!("\t after {:?}", self.workspace.lock().await);
    }
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
    #[allow(unused)]
    async fn use_llm<F>(&self, proc: F) -> core::result::Result<String, LlmError>
    where
        F: for<'b, 'a> AsyncFnOnce(
            &'b mut Box<dyn LlmClient + 'a>,
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
            &'b mut Box<dyn LlmClient + 'a>,
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
            if let Some(v) = option.get("reasoning_effort")
                && let Ok(n) = v.parse::<ReasoningEffort>()
            {
                llm.reasoning_effort(n);
            }
            // if let Some(v) = data.get("response_format") { ... }
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

            return proc(llm).await;
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
    async fn init_workspace(&self, mut workspaces: Vec<WorkspaceFolder>) {
        debug!("init_workspace: {:?}", workspaces);
        self.workspace.lock().await.append(&mut workspaces);
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
        .format_target(false)
        .format_module_path(false)
        .format_source_path(true)
        .target(env_logger::Target::Stderr)
        .init();

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    info!("initialize lsp service");
    let none: Option<Box<dyn LlmClient>> = None;
    let (service, socket) = tower_lsp::LspService::build(|client| Backend {
        client,
        text: DashMap::new(),
        workspace: tokio::sync::Mutex::new(vec![]),
        llm: tokio::sync::Mutex::new(none),
        highlighter: Highlighter::new(),
        db: FlightRecorder::new(&path.join("data").join("fifty_four.db")),
        character_cache: CharacterCache::new(),
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
    const CHARACTERS_MD: &str = indoc!(
        "# キャラクター記述スタイルガイド
        ## ジェフ・クライン（艦長）
        ### 背景・立場
        - ムサイ艦の艦長。元警備隊員で予備役上がり。
        ### 性格・口調
        - 落ち着いていて経験豊富。
        - 若手を気遣う姿勢があり、柔らかい口調で励ます。
        - 軽い冗談や皮肉も交えるが、威圧的ではない。
        ### 描写
        - 内省的なモノローグを交えることで、過去の経緯や感情を表現。
        - 視点人物として描かれることが多く、周囲の状況や人物への観察が豊富。
        - 軍務に対する冷静な視点と、個人的な感慨が混在する。
        ### 外見・その他
        - 明確な外見描写はなし。
        - フォン・ブラウン出身、サイド3に移住経験あり。
        ## シルビア（航海士）
        ### 背景・立場
        - 若手の航海士。高校を飛び出して促成コースで軍に入隊。
        ### 性格・口調
        - 真面目で緊張しやすいが、素直で礼儀正しい。
        - 敬語を使い、上官に対して忠実。
        ### 描写
        - 若さと未熟さを強調する描写（肩に力が入る、敬礼、緊張）。
        - 操縦技術や成長の兆しを描くことで、読者に期待感を持たせる。
        - 艦長との対話で人間関係や信頼感を表現。
        ### 外見・その他
        - 「少女」と形容される。
        - 操縦桿を握る姿勢や伸びをする仕草など、身体的な動作描写が多い。
        "
    );

    fn make_entry(md: &str) -> FileCacheEntry {
        FileCacheEntry {
            modified: std::time::SystemTime::UNIX_EPOCH,
            characters: parse_all_content(md),
        }
    }

    #[test]
    fn test_parse_characters_md_detect_level() {
        let chars = parse_all_content(CHARACTERS_MD);
        // level 2 がキャラクターレベルとして正しく検出されること
        assert!(
            chars.contains_key("ジェフ・クライン（艦長）"),
            "キーが存在しない: {:?}",
            chars.keys().collect::<Vec<_>>()
        );
        assert!(chars.contains_key("シルビア（航海士）"));
    }

    #[test]
    fn test_parse_characters_md_background() {
        let entry = make_entry(CHARACTERS_MD);
        let result = CharacterInfoTool::search_cache(&entry, "クライン", &["背景".to_string()]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().contains("予備役"));
    }

    #[test]
    fn test_parse_characters_md_expression() {
        let entry = make_entry(CHARACTERS_MD);
        let result = CharacterInfoTool::search_cache(&entry, "シルビア", &["描写".to_string()]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().contains("成長の兆し"));
    }

    #[test]
    fn test_parse_characters_md_personality() {
        let entry = make_entry(CHARACTERS_MD);
        let result = CharacterInfoTool::search_cache(&entry, "ジェフ", &["性格".to_string()]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().starts_with("落ち着いていて"));
    }

    #[test]
    fn test_parse_characters_md_multi_tag_from_heading() {
        // "性格・口調" heading が ["性格", "口調"] に分割されること
        let entry = make_entry(CHARACTERS_MD);
        let by_kuchou = CharacterInfoTool::search_cache(&entry, "ジェフ", &["口調".to_string()]);
        assert!(
            by_kuchou.is_ok(),
            "「口調」タグでヒットしない: {:?}",
            by_kuchou
        );
        let by_seikaku = CharacterInfoTool::search_cache(&entry, "ジェフ", &["性格".to_string()]);
        assert_eq!(
            by_kuchou.unwrap(),
            by_seikaku.unwrap(),
            "「口調」と「性格」は同じセクションを返すはず"
        );
    }

    #[test]
    fn test_parse_characters_md_or_search() {
        // 複数タグ OR 検索: 異なるセクションがまとめて返ること
        let entry = make_entry(CHARACTERS_MD);
        let result = CharacterInfoTool::search_cache(
            &entry,
            "クライン",
            &["背景".to_string(), "性格".to_string()],
        );
        assert!(result.is_ok(), "{:?}", result);
        let text = result.unwrap();
        assert!(text.contains("予備役"), "背景セクションが含まれていない");
        assert!(
            text.contains("落ち着いていて"),
            "性格セクションが含まれていない"
        );
    }

    #[test]
    fn test_parse_characters_md_failure() {
        let entry = make_entry(CHARACTERS_MD);
        let result = CharacterInfoTool::search_cache(&entry, "ユルゲン", &["性格".to_string()]);
        assert!(result.is_err(), "存在しないキャラクターでエラーにならない");
    }
}
