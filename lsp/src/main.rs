// シンプルな LSP サーバの実装例（tower-lsp を利用）
// このファイルは最小限の動作をする "何もしない" サーバを提供します。
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, options};
use dashmap::mapref::one::RefMut;
use std::ffi::OsStr;
use std::sync::Arc;
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer, UriExt};
// use tracing::{debug, info, instrument, span, warn};
mod highlight;
use crate::highlight::Highlighter;
mod logging;
mod types;
use crate::types::{CursorContext, LineData};
use dashmap::DashMap;
use std::collections::HashMap;
use std::str::FromStr;
mod character_updater;
use crate::character_updater::UpdateState;
mod cursor_context;
mod llm;
mod tools;
use crate::llm::{Content, LlmClientBuilder, LlmError, LlmInterface, ModelCapability};
use genai::chat::{ChatResponseFormat, JsonSpec};
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
pub(crate) struct FlightRecorder {
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

    fn record_character_update(&self, uri: &str, model: &str, prompt: &str) -> i64 {
        let db = self.conn.lock();
        db.query_row(
            "INSERT INTO character_updates (document_uri, model_name, prompt) VALUES (?,?,?) RETURNING id;",
            rusqlite::params![uri, model, prompt],
            |row| row.get(0),
        ).unwrap_or(-1)
    }

    fn record_character_response(&self, update_id: i64, response: &str) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "UPDATE character_updates SET response = ? WHERE id = ?;",
            rusqlite::params![response, update_id],
        ) {
            debug!("record_character_response failed: {}", e);
        }
    }

    fn record_character_section(
        &self,
        update_id: i64,
        name: &str,
        attr: &str,
        old_text: Option<&str>,
        new_text: &str,
        applied: bool,
        skip_reason: Option<&str>,
    ) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "INSERT INTO character_update_sections (update_id, character_name, attribute, old_text, new_text, applied, skip_reason) VALUES (?,?,?,?,?,?,?);",
            rusqlite::params![update_id, name, attr, old_text, new_text, applied, skip_reason],
        ) {
            debug!("record_character_section failed: {}", e);
        }
    }

    fn complete_character_update(&self, update_id: i64) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "UPDATE character_updates SET completed_at = datetime('now', 'subsec') WHERE id = ?;",
            rusqlite::params![update_id],
        ) {
            debug!("complete_character_update failed: {}", e);
        }
    }
}

#[cfg(not(debug_assertions))]
#[derive(Debug)]
pub(crate) struct FlightRecorder {}

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

    fn record_character_update(&self, _uri: &str, _model: &str, _prompt: &str) -> i64 {
        -1
    }
    fn record_character_response(&self, _update_id: i64, _response: &str) {}
    fn record_character_section(
        &self,
        _update_id: i64,
        _name: &str,
        _attr: &str,
        _old_text: Option<&str>,
        _new_text: &str,
        _applied: bool,
        _skip_reason: Option<&str>,
    ) {
    }
    fn complete_character_update(&self, _update_id: i64) {}
}

#[derive(Debug, PartialEq, Clone)]
pub enum CharacterAttribute {
    Appearance,
    Background,
    Expression,
    Personality,
    Relationship,
    Role,
    Style,
    Weakness,
    /// 呼称・別名（人名ハイライトの絞り込みに使う aliases の抽出元）
    Alias,
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
            "alias" | "呼称" | "別名" | "通称" | "あだ名" | "渾名" | "二つ名" | "愛称"
            | "呼び名" | "異名" => Ok(Self::Alias),
            _ => Err(format!("No such attribute {}", s)),
        }
    }
}

impl CharacterAttribute {
    /// 新規セクション/ファイル作成時に使う日本語正規見出しを返す。
    pub fn canonical_heading(&self) -> &'static str {
        match self {
            Self::Appearance => "容姿",
            Self::Background => "背景",
            Self::Expression => "口調",
            Self::Personality => "性格",
            Self::Relationship => "関係",
            Self::Role => "役割",
            Self::Style => "描写",
            Self::Weakness => "弱点",
            Self::Alias => "呼称",
        }
    }
}

/// タグ付きコンテンツ。1つの見出しセクション（属性）に対応する。
#[derive(Debug, Clone)]
pub(crate) struct TaggedContent {
    /// 見出しを「・」で分割したタグ群
    pub(crate) tags: Vec<CharacterAttribute>,
    /// セクションのプレーンテキスト
    pub(crate) text: String,
}

/// 1キャラクター分のキャッシュ
#[derive(Debug, Clone)]
pub(crate) struct CharacterEntry {
    pub(crate) sections: Vec<TaggedContent>,
    /// `Alias` タグ付きセクションから抽出済みの別名リスト（人名ハイライトの絞り込みに使う）
    pub(crate) aliases: Vec<String>,
}

/// 1ファイル分のキャッシュ
#[derive(Debug)]
pub(crate) struct FileCacheEntry {
    pub(crate) modified: std::time::SystemTime,
    /// key: heading 全文（部分一致検索用）
    pub(crate) characters: HashMap<String, CharacterEntry>,
}

/// 全ファイルのキャッシュ (Arc で複数の CharacterInfoTool インスタンス間で共有)
#[derive(Debug, Clone)]
pub(crate) struct CharacterCache(
    pub(crate) Arc<parking_lot::Mutex<HashMap<PathBuf, FileCacheEntry>>>,
);

impl CharacterCache {
    fn new() -> Self {
        Self(Arc::new(parking_lot::Mutex::new(HashMap::new())))
    }
}

/// workspace 内でキャラクター名に対応するファイルパスを解決する自由関数。
pub fn find_character_file_path(
    workspace: &Path,
    name: &str,
) -> std::result::Result<PathBuf, LlmError> {
    let single = workspace.join("characters").with_extension("md");
    trace!("{:?}", &single);
    if single.exists() && single.is_file() {
        return Ok(single);
    }

    let dir = workspace.join("characters");
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

/// ファイル内の heading 構造からキャラクターを表す heading レベルを推定する。
///
/// 各 heading レベルの出現回数と「直後に level+1 の heading が続くか」を調べ、
/// 最も出現回数が多い「子持ちレベル」を返す。タイ時は深レベル(より多くの # を持つ)優先。
/// 例: `# Story / ## キャラ / ### 属性` のように各レベルが 1 件ずつの場合、
/// タイトル(1)よりキャラクター(2)を選ぶ方が意味的に正しい。
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
        .max_by(|(l1, c1), (l2, c2)| c1.cmp(c2).then(l1.cmp(l2)))
        .map(|(l, _)| *l)
        .unwrap_or(0)
}

/// このプロジェクト共通の comrak パースオプションを返す。
fn comrak_options() -> comrak::Options<'static> {
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
    options
}

/// Markdown 文字列からキャラクターレベルを推定する。
///
/// `detect_char_level` の文字列入力版。`character_updater` 等で共有する。
/// 検出できない場合は `0` を返す。
pub(crate) fn detect_char_level_str(text: &str) -> u8 {
    let arena = Arena::new();
    let options = comrak_options();
    let root = comrak::parse_document(&arena, text, &options);
    detect_char_level(root)
}

/// Markdown 文字列をパースし、全キャラクターの全セクションを `HashMap` で返す。
///
/// キーは heading 全文（例: "ジェフ・クライン（艦長）"）。
/// 属性 heading のテキストを「・」で分割してタグ群を生成する。
pub fn parse_all_content(content: &str) -> HashMap<String, CharacterEntry> {
    let arena = Arena::new();
    let options = comrak_options();

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
                    let is_alias = section.tags.contains(&CharacterAttribute::Alias);
                    let entry = characters
                        .entry(char_name.to_string())
                        .or_insert_with(|| CharacterEntry {
                            sections: Vec::new(),
                            aliases: Vec::new(),
                        });
                    if is_alias {
                        entry.aliases.extend(split_aliases(&section.text));
                    }
                    entry.sections.push(section);
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

/// `Alias` タグ付きセクションのテキストを個々の別名に分割する。
/// 箇条書きの各行をさらに区切り文字（「・」「、」「,」「/」半角スペース）で分割し、
/// trim・空文字列除外して返す。
fn split_aliases(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| line.split(['・', '、', ',', '/', ' ']))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// キャラクターの見出しキーから役職等の括弧書きを除いた表示名を返す。
/// 例: "ジェフ・クライン（艦長）" -> "ジェフ・クライン"
fn character_display_name(heading_key: &str) -> &str {
    let end = heading_key
        .find(['（', '('])
        .unwrap_or(heading_key.len());
    heading_key[..end].trim()
}

/// `character_cache` 全体から、人名ハイライトの絞り込みに使う許可名集合を構築する。
/// 各キャラの表示名・aliasesに加え、「・」区切りの複合名は各部分も対象に含める
/// （例: "ジェフ・クライン" -> "ジェフ・クライン","ジェフ","クライン"）。
/// 1文字の断片は助詞等との誤マッチが起きやすいため除外する。
pub(crate) fn collect_allowed_names(cache: &CharacterCache) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();

    let mut add_name = |n: &str| {
        let n = n.trim();
        if n.chars().count() >= 2 {
            names.insert(n.to_string());
        }
    };

    let guard = cache.0.lock();
    for file_entry in guard.values() {
        for heading_key in file_entry.characters.keys() {
            let display_name = character_display_name(heading_key);
            add_name(display_name);
            for part in display_name.split('・') {
                add_name(part);
            }
        }
        for entry in file_entry.characters.values() {
            for alias in &entry.aliases {
                add_name(alias);
                for part in alias.split('・') {
                    add_name(part);
                }
            }
        }
    }

    names
}

/// 1キャラクター分の全セクションを Markdown 文字列に整形する(ホバー表示用)。
/// `search_cache`(tools.rs)と異なりタグによる絞り込みは行わず、全セクションを出力する。
fn character_entry_to_markdown(heading_key: &str, entry: &CharacterEntry) -> String {
    let mut out = format!("# {}", heading_key);

    for section in &entry.sections {
        let text = section.text.trim();
        if text.is_empty() {
            continue;
        }
        if section.tags.is_empty() {
            // 見出しが CharacterAttribute に認識されなかったセクション。
            // 元の生見出し文字列は保持していないため、誤ったラベルを付けるより本文のみ出す。
            out.push_str(&format!("\n\n{}", text));
        } else {
            let heading = section
                .tags
                .iter()
                .map(|t| t.canonical_heading())
                .collect::<Vec<_>>()
                .join("・");
            out.push_str(&format!("\n\n## {}\n{}", heading, text));
        }
    }

    out
}

/// `character_cache` 全体を横断し、`surface`(表示名/「・」分割部分名/alias)に一致する
/// キャラクターを探して Markdown 化した説明文を返す(ホバー表示用)。
/// 一致判定は `collect_allowed_names` と同じルールを逆向きに適用する。
/// 同名のキャラが複数ファイルに存在する場合は "---" 区切りで連結して返す。
fn lookup_character_markdown(cache: &CharacterCache, surface: &str) -> Option<String> {
    if surface.chars().count() < 2 {
        return None;
    }

    let matches_surface = |heading_key: &str, entry: &CharacterEntry| -> bool {
        let display_name = character_display_name(heading_key);
        if display_name == surface || display_name.split('・').any(|p| p == surface) {
            return true;
        }
        entry
            .aliases
            .iter()
            .any(|a| a == surface || a.split('・').any(|p| p == surface))
    };

    let guard = cache.0.lock();
    let hits: Vec<String> = guard
        .values()
        .flat_map(|file_entry| file_entry.characters.iter())
        .filter(|(heading_key, entry)| matches_surface(heading_key, entry))
        .map(|(heading_key, entry)| character_entry_to_markdown(heading_key, entry))
        .collect();
    drop(guard);

    if hits.is_empty() {
        None
    } else {
        Some(hits.join("\n\n---\n\n"))
    }
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

pub(crate) fn shorten_middle(s: &str, len: usize) -> String {
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
}

fn build_llm_client(cfg: &serde_json::Value) -> Box<dyn LlmInterface> {
    // prompt 群と同じく exe 隣の data/ を優先し、無ければ埋め込みアセットへフォールバック
    // (デバッグビルドの埋め込みはビルドマシンの絶対パスを実行時に読むため、転送先では失敗する)。
    let sys_prompt = load_prompt_from_disk("system.md").or_else(|| {
        Asset::get("system.md").map(|d| String::from_utf8_lossy(d.data.as_ref()).to_string())
    });
    if sys_prompt.is_none() {
        warn!("system.md not found on disk nor in embedded assets; LLM runs without system prompt");
    }
    LlmClientBuilder::from_value(cfg).sys_prompt(sys_prompt).build()
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
                if let Some(cfg) = llm_root
                    .get("deferred")
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
                warn!("did_change_watched_files: failed to convert uri to file path: {:?}", change.uri);
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

        self.highlighter.initialize();

        let vec = {
            let mut lines = self.text.get(uri).expect("Failed to get text").to_vec();
            let tokens = lines
                .iter_mut()
                .map(|l| self.highlighter.tokenize(l))
                .collect::<Vec<_>>();
            Highlighter::to_semantic_tokens(tokens)
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
    async fn hover(
        &self,
        params: HoverParams,
    ) -> tower_lsp_server::jsonrpc::Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        let uri = pos.text_document.uri.as_str();
        let line_no = pos.position.line as usize;
        let utf16_offset = pos.position.character as usize;

        self.highlighter.initialize();

        let mut tmp: RefMut<_, _> = match self.text.try_get_mut(uri) {
            TryResult::Locked | TryResult::Absent => return Ok(None),
            TryResult::Present(t) => t,
        };
        if line_no >= tmp.len() {
            return Ok(None);
        }

        let highlighter = &self.highlighter;
        let hit = cursor_context::token_at(
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

            let highlighter = &self.highlighter;
            let context = cursor_context::classify_complesion_mode(
                tmp.as_mut_slice(),
                line_no,
                offset,
                |line| {
                    line.tokens = highlighter.text_to_lindera_token(line.text.as_str());
                },
            );
            (context, before)
        };

        let prompt_fn = Backend::ctx_to_prompt_name(context);
        debug!("Prompt: {}", prompt_fn);
        let (prompt, options) =
            crate::load_prompt(prompt_fn).unwrap_or_else(|| panic!("{} not found", prompt_fn));

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

        let mut completion_id = 0u32;
        let raw = self
            .use_llm_with_option(options, async |l| {
                l.add_tool(tools::CharacterInfoTool::new(
                    workspace,
                    self.character_cache.clone(),
                ));
                l.add_tool(tools::PlotInfoTool::new(workspace));
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

                Err(tower_lsp_server::jsonrpc::Error::invalid_params(err.to_string()))
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
            self.workspace.lock().await.deref_mut().push(path.into_owned());
        }
        debug!("\t after {:?}", self.workspace.lock().await);
    }
}

fn apply_changes<T: AsRef<str>>(lines: &mut Vec<LineData>, text: T, range: Range) {
    let start_line = range.start.line as usize;
    let start_char = range.start.character as usize;
    let end_line = range.end.line as usize;
    let end_char = range.end.character as usize;

    // range.character は UTF-16 コード単位(positionEncoding=utf-16)。
    // utf16_to_byte_offset が行末クランプを内包する。
    let prefix = lines
        .get(start_line)
        .map(|l| {
            let ix = crate::types::utf16_to_byte_offset(&l.text, start_char);
            l.text[..ix].to_string()
        })
        .unwrap_or("".to_string());
    let suffix = lines
        .get(end_line)
        .map(|l| {
            let ix = crate::types::utf16_to_byte_offset(&l.text, end_char);
            l.text[ix..].to_string()
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
    async fn refresh_highlight_names(&self) {
        let names = collect_allowed_names(&self.character_cache);
        debug!("refresh_highlight_names: {} 件の許可名", names.len());
        self.highlighter.set_allowed_names(names);
        if let Err(e) = self.client.semantic_tokens_refresh().await {
            warn!("semantic_tokens_refresh failed: {:?}", e);
        }
    }

    /// did_change イベントの情報を更新状態に記録し、発火判定を行う。
    /// 判定は常にこのタイミングのみ(周期タスクは持たない)。
    /// - 現在の変更を取り込む前に直前バーストの idle 判定(`idle_trigger`)を行い、
    ///   idle 確定なら `min_chars` 以上で発火・未満でクリア。
    /// - 現在の変更を取り込んだ後、`max_chars` 以上なら即時発火。
    /// 発火時に spawn する非同期タスクは `character_updater::run` だけ。
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
                character_updater::idle_trigger(s.accumulated_chars, gap, idle, min_chars);
            debug!(
                "record_change[{}]: idle_trigger -> {:?} (accumulated={}, gap={:?}, idle={:?}, min={}, max={})",
                uri, trigger, s.accumulated_chars, gap, idle, min_chars, max_chars
            );
            match trigger {
                character_updater::Trigger::Fire => {
                    fire_text = Some(character_updater::full_text(&self.text, uri));
                    s.reset();
                    s.running = true;
                    apply(&mut s); // 現在の変更で新バーストを開始
                    debug!(
                        "record_change[{}]: FIRE (idle), new burst started with {} chars",
                        uri, s.accumulated_chars
                    );
                }
                character_updater::Trigger::ClearStale => {
                    s.reset();
                    apply(&mut s);
                    debug!(
                        "record_change[{}]: ClearStale, restart burst with {} chars",
                        uri, s.accumulated_chars
                    );
                }
                character_updater::Trigger::None => {
                    // 2. 打鍵継続中: 現在の変更を取り込み max_chars 即時発火を判定。
                    apply(&mut s);
                    if s.accumulated_chars >= max_chars {
                        fire_text = Some(character_updater::full_text(&self.text, uri));
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
                "record_change[{}]: spawning character_updater::run ({} chars full text)",
                uri,
                text.chars().count()
            );
            tokio::spawn(character_updater::run(
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

#[derive(Embed)]
#[folder = "../data/"]
struct Asset;

/// 埋め込みアセットからプロンプトを読み込み、YAML frontmatter を本文と分離して返す。
///
/// - アセットが見つからない場合は `None`(欠如時の扱いは呼び出し側に委ねる)。
/// - frontmatter が無い・パースに失敗した場合は本文をそのまま・空の data を返す。
pub(crate) fn load_prompt(name: &str) -> Option<(String, HashMap<String, String>)> {
    let template: String = match load_prompt_from_disk(name) {
        Some(s) => s,
        None => {
            let asset = Asset::get(name)?;
            String::from_utf8_lossy(asset.data.as_ref()).into_owned()
        }
    };
    let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
    // いったん Pod として受け、スカラー値を文字列へ正規化する。
    // HashMap<String, String> へ直接デシリアライズすると、frontmatter に数値や真偽値が
    // 1つでもあると(例: `max_tokens: 4096`)全体のパースが失敗し、`schema` 等も巻き添えで
    // 失われてしまうため(option は use_llm_with_option 側で文字列から parse し直す前提)。
    let parsed = match matter.parse::<gray_matter::Pod>(&template) {
        Ok(p) => p,
        Err(_) => return Some((template, HashMap::new())),
    };
    let body = parsed.content;
    let mut data = HashMap::new();
    if let Some(gray_matter::Pod::Hash(hash)) = parsed.data {
        for (k, v) in hash {
            if let Some(s) = pod_to_string(&v) {
                data.insert(k, s);
            }
        }
    }
    Some((body, data))
}

/// 実行ファイルと同じディレクトリの `data/<name>` があれば、そちらを優先して読む
/// (再ビルドせずにプロンプトを編集して試すための dev 用フック)。無ければ `None`。
fn load_prompt_from_disk(name: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    std::fs::read_to_string(dir.join("data").join(name)).ok()
}

/// frontmatter の Pod を文字列へ正規化する。
///
/// スカラーはそのまま文字列化し、配列・オブジェクト(JSON schema 等)は JSON 文字列に変換する。
/// Null のみ `None`(マップから除外)。
fn pod_to_string(pod: &gray_matter::Pod) -> Option<String> {
    use gray_matter::Pod;
    match pod {
        Pod::String(s) => Some(s.clone()),
        Pod::Integer(n) => Some(n.to_string()),
        Pod::Float(f) => Some(f.to_string()),
        Pod::Boolean(b) => Some(b.to_string()),
        Pod::Null => None,
        Pod::Array(_) | Pod::Hash(_) => serde_json::to_string(&pod_to_json_value(pod)).ok(),
    }
}

fn pod_to_json_value(pod: &gray_matter::Pod) -> serde_json::Value {
    use gray_matter::Pod;
    match pod {
        Pod::Null => serde_json::Value::Null,
        Pod::String(s) => serde_json::Value::String(s.clone()),
        Pod::Integer(n) => serde_json::json!(n),
        Pod::Float(f) => serde_json::json!(f),
        Pod::Boolean(b) => serde_json::Value::Bool(*b),
        Pod::Array(items) => {
            serde_json::Value::Array(items.iter().map(pod_to_json_value).collect())
        }
        Pod::Hash(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), pod_to_json_value(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

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

    // .env はローカル開発専用(debugビルドのみ)なので、ソースリポジトリのルート基準で読む。
    // 無い場合(ビルドマシン外へ転送したデバッグビルド等)は OS の環境変数をそのまま使う。
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // 環境変数の初期化
    #[cfg(debug_assertions)]
    {
        if let Err(err) = dotenvx_rs::dotenvx::from_path(manifest_dir.join(".env")) {
            eprintln!(
                "No .env loaded ({}); using process environment variables as-is",
                err
            );
        }
    }

    env_logger::Builder::from_default_env()
        .format_target(false)
        .format_module_path(false)
        .format_source_path(true)
        .target(env_logger::Target::Stderr)
        .init();

    // db/ は実行ファイル自身の隣に置く(コピー先のフォルダでもそのまま動くように、
    // ビルド時に固定される CARGO_MANIFEST_DIR ではなく実行時の current_exe を基準にする)。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| manifest_dir.to_path_buf());

    // LspService を構築し、`Backend` をクライアントハンドルで初期化する
    info!("initialize lsp service");
    let db_dir = exe_dir.join("db");
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        eprintln!("Failed to create db directory {:?}: {}", db_dir, e);
        return;
    }
    let db_path = db_dir.join("fifty_four.db");
    let (service, socket) = tower_lsp_server::LspService::build(|client| Backend {
        client,
        text: DashMap::new(),
        workspace: Arc::new(tokio::sync::Mutex::new(vec![])),
        llm: Arc::new(tokio::sync::Mutex::new(None)),
        background_llm: Arc::new(tokio::sync::Mutex::new(None)),
        character_updater_enabled: std::sync::atomic::AtomicBool::new(true),
        character_updater_min_chars: std::sync::atomic::AtomicUsize::new(
            character_updater::DEFAULT_MIN_CHARS,
        ),
        character_updater_max_chars: std::sync::atomic::AtomicUsize::new(
            character_updater::DEFAULT_MAX_CHARS,
        ),
        character_updater_idle_secs: std::sync::atomic::AtomicU64::new(
            character_updater::DEFAULT_IDLE_SECS,
        ),
        highlighter: Highlighter::new(),
        db: Arc::new(FlightRecorder::new(&db_path)),
        character_cache: CharacterCache::new(),
        update_states: DashMap::new(),
    })
    .finish();

    // サーバを起動してクライアントとのメッセージループを開始する
    info!("start server");

    tower_lsp_server::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;

    // drop(logger);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CharacterInfoTool;
    use indoc::indoc;
    use tower_lsp_server::lsp_types::{Position, Range};

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

    /// load_prompt と同じ frontmatter 正規化を文字列入力で再現する(Asset 非依存のテスト用)。
    fn parse_frontmatter(template: &str) -> HashMap<String, String> {
        let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
        let parsed = matter.parse::<gray_matter::Pod>(template).unwrap();
        let mut data = HashMap::new();
        if let Some(gray_matter::Pod::Hash(hash)) = parsed.data {
            for (k, v) in hash {
                if let Some(s) = pod_to_string(&v) {
                    data.insert(k, s);
                }
            }
        }
        data
    }

    // frontmatter に数値・真偽値が混在しても、文字列フィールド(schema 等)が
    // 巻き添えで失われないこと(EOF schema バグの回帰防止)。
    #[test]
    fn test_frontmatter_mixed_scalar_types() {
        let fm = parse_frontmatter(indoc!(
            r#"---
            schema: >
              {"type":"object"}
            schema_name: character_updates
            max_tokens: 4096
            temperature: 0.3
            enabled: true
            ---
            body text"#
        ));
        // 数値の隣にあっても schema(文字列)が生き残る
        assert_eq!(
            fm.get("schema").map(|s| s.trim()),
            Some(r#"{"type":"object"}"#)
        );
        assert_eq!(
            fm.get("schema_name").map(|s| s.as_str()),
            Some("character_updates")
        );
        // 数値・真偽値は文字列化されて取得できる(use_llm_with_option が parse し直す)
        assert_eq!(fm.get("max_tokens").map(|s| s.as_str()), Some("4096"));
        assert_eq!(fm.get("temperature").map(|s| s.as_str()), Some("0.3"));
        assert_eq!(fm.get("enabled").map(|s| s.as_str()), Some("true"));
    }

    #[test]
    fn test_pod_to_string() {
        use gray_matter::Pod;
        assert_eq!(pod_to_string(&Pod::Integer(42)), Some("42".to_string()));
        assert_eq!(
            pod_to_string(&Pod::Boolean(false)),
            Some("false".to_string())
        );
        assert_eq!(
            pod_to_string(&Pod::String("x".into())),
            Some("x".to_string())
        );
        assert_eq!(pod_to_string(&Pod::Null), None);
        assert_eq!(pod_to_string(&Pod::Array(vec![])), Some("[]".to_string()));
        let mut schema = Pod::new_hash();
        schema["type"] = Pod::String("object".into());
        let s = pod_to_string(&schema).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "object");
    }

    // schema を YAML オブジェクトとして書いた場合も JSON 文字列として取得できる。
    #[test]
    fn test_frontmatter_schema_as_yaml_object() {
        let fm = parse_frontmatter(indoc!(
            r#"---
            schema:
              type: object
              properties:
                updates:
                  type: array
            schema_name: character_updates
            max_tokens: 4096
            ---
            body"#
        ));
        let schema_str = fm.get("schema").expect("schema missing");
        let schema: serde_json::Value = serde_json::from_str(schema_str).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["updates"]["type"], "array");
        assert_eq!(fm.get("max_tokens").map(|s| s.as_str()), Some("4096"));
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

    #[test]
    fn test_apply_changes_utf16_supplementary() {
        // "𠮷田" (𠮷=サロゲートペア,UTF-16で2単位/田=1単位)。
        // character=2 は 𠮷 の直後(UTF-16基準)を指す。
        // 旧char単位実装では chars().take(2) = "𠮷田" 全体が prefix になり
        // "!" が末尾(田の後ろ)に入ってしまっていた誤りを検証する。
        let mut ls = lines("𠮷田");
        apply_changes(&mut ls, "!", range(0, 2, 0, 2));
        assert_eq!(ls, lines("𠮷!田"));
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

    #[test]
    fn test_alias_attribute_try_from() {
        assert_eq!(
            CharacterAttribute::try_from("呼称"),
            Ok(CharacterAttribute::Alias)
        );
        assert_eq!(
            CharacterAttribute::try_from("別名"),
            Ok(CharacterAttribute::Alias)
        );
        assert_eq!(
            CharacterAttribute::try_from("通称"),
            Ok(CharacterAttribute::Alias)
        );
    }

    #[test]
    fn test_parse_aliases_from_heading() {
        const MD: &str = indoc!(
            "## ジェフ・クライン（艦長）
            ### 呼称
            - ジェフ
            - クライン艦長・隊長
            ### 背景・立場
            - ムサイ艦の艦長。
            "
        );
        let chars = parse_all_content(MD);
        let entry = chars
            .get("ジェフ・クライン（艦長）")
            .expect("キャラが見つからない");
        assert!(
            entry.aliases.contains(&"ジェフ".to_string()),
            "{:?}",
            entry.aliases
        );
        assert!(
            entry.aliases.contains(&"クライン艦長".to_string()),
            "{:?}",
            entry.aliases
        );
        assert!(
            entry.aliases.contains(&"隊長".to_string()),
            "{:?}",
            entry.aliases
        );
        // sections には元テキストもそのまま残っていること(後方互換・検索用)
        assert!(
            entry
                .sections
                .iter()
                .any(|s| s.tags.contains(&CharacterAttribute::Alias))
        );
    }

    #[test]
    fn test_character_display_name() {
        assert_eq!(
            character_display_name("ジェフ・クライン（艦長）"),
            "ジェフ・クライン"
        );
        assert_eq!(character_display_name("シルビア（航海士）"), "シルビア");
        assert_eq!(character_display_name("役職なしキャラ"), "役職なしキャラ");
    }

    #[test]
    fn test_collect_allowed_names() {
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(CHARACTERS_MD));
        let names = collect_allowed_names(&cache);

        // フルネーム・「・」分割された部分名が含まれること
        assert!(names.contains("ジェフ・クライン"), "{:?}", names);
        assert!(names.contains("ジェフ"), "{:?}", names);
        assert!(names.contains("クライン"), "{:?}", names);
        assert!(names.contains("シルビア"), "{:?}", names);

        // alias未登録の役職名は含まれないこと
        assert!(!names.contains("艦長"), "{:?}", names);
        assert!(!names.contains("航海士"), "{:?}", names);
    }

    #[test]
    fn test_collect_allowed_names_includes_aliases() {
        const MD: &str = indoc!(
            "## ジェフ・クライン（艦長）
            ### 呼称
            - 隊長
            "
        );
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(MD));
        let names = collect_allowed_names(&cache);
        assert!(names.contains("隊長"), "{:?}", names);
    }

    #[test]
    fn test_character_entry_to_markdown() {
        let chars = parse_all_content(CHARACTERS_MD);
        let entry = chars
            .get("ジェフ・クライン（艦長）")
            .expect("キャラが見つからない");
        let md = character_entry_to_markdown("ジェフ・クライン（艦長）", entry);

        assert!(md.starts_with("# ジェフ・クライン（艦長）"), "{}", md);
        // 全セクション(抜粋しない)が含まれること。見出しは複数タグが「・」結合されることがあるため
        // (例: "背景・立場"→タグ[Background,Role]→"背景・役割")、部分文字列で緩く検証する。
        assert!(md.contains("背景"), "{}", md);
        assert!(md.contains("予備役"), "{}", md);
        assert!(md.contains("口調"), "{}", md);
        assert!(md.contains("落ち着いていて"), "{}", md);
        assert!(md.contains("## 描写"), "{}", md);
        assert!(md.contains("## 容姿"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_display_name() {
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(CHARACTERS_MD));
        let md = lookup_character_markdown(&cache, "ジェフ・クライン")
            .expect("表示名で見つかるはず");
        assert!(md.contains("予備役"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_split_part() {
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(CHARACTERS_MD));
        let md = lookup_character_markdown(&cache, "クライン")
            .expect("「・」分割部分名で見つかるはず");
        assert!(md.contains("予備役"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_alias() {
        const MD: &str = indoc!(
            "## ジェフ・クライン（艦長）
            ### 呼称
            - 隊長
            ### 背景・立場
            - ムサイ艦の艦長。
            "
        );
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(MD));
        let md = lookup_character_markdown(&cache, "隊長").expect("aliasで見つかるはず");
        assert!(md.contains("艦長"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_miss() {
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(CHARACTERS_MD));
        assert!(lookup_character_markdown(&cache, "存在しない人").is_none());
        // 1文字はノイズ除外(collect_allowed_namesと同じ基準)で必ずNone
        assert!(lookup_character_markdown(&cache, "ジ").is_none());
    }

    #[test]
    fn test_lookup_character_markdown_cross_file() {
        const MD_A: &str = indoc!(
            "## アリス（技師）
            ### 背景・立場
            - 整備班所属。
            "
        );
        const MD_B: &str = indoc!(
            "## ボブ（通信士）
            ### 背景・立場
            - 通信班所属。
            "
        );
        let cache = CharacterCache::new();
        {
            let mut guard = cache.0.lock();
            guard.insert(PathBuf::from("a.md"), make_entry(MD_A));
            guard.insert(PathBuf::from("b.md"), make_entry(MD_B));
        }

        let md_a = lookup_character_markdown(&cache, "アリス").expect("a.mdのキャラが見つかるはず");
        assert!(md_a.contains("整備班"), "{}", md_a);
        let md_b = lookup_character_markdown(&cache, "ボブ").expect("b.mdのキャラが見つかるはず");
        assert!(md_b.contains("通信班"), "{}", md_b);
    }

    #[test]
    fn test_lookup_character_markdown_same_name_multiple_files_joined() {
        const MD_A: &str = indoc!(
            "## タナカ（技師）
            ### 背景・立場
            - A船の整備士。
            "
        );
        const MD_B: &str = indoc!(
            "## タナカ（通信士）
            ### 背景・立場
            - B船の通信士。
            "
        );
        let cache = CharacterCache::new();
        {
            let mut guard = cache.0.lock();
            guard.insert(PathBuf::from("a.md"), make_entry(MD_A));
            guard.insert(PathBuf::from("b.md"), make_entry(MD_B));
        }

        let md = lookup_character_markdown(&cache, "タナカ").expect("両ファイルのタナカが見つかるはず");
        assert!(md.contains("A船の整備士"), "{}", md);
        assert!(md.contains("B船の通信士"), "{}", md);
        assert!(md.contains("---"), "{} に区切り線が含まれるはず", md);
    }
}
