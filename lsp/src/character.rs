//! キャラクター設定ファイル(`characters.md` 等)のドメインモデルと Markdown パーサ。
//!
//! `main.rs` から切り出したモジュール。`Backend`(LSP ハンドラ)と
//! `character_updater`(自動更新)の双方から参照される、キャラクター情報の
//! 「読み取り」側を担う。書き込み・マージロジックは `character_updater` 側にある。

use crate::llm::LlmError;
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, options};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[allow(unused_imports)]
use log::{debug, trace};

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum CharacterAttribute {
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
            "alias" | "aliases" | "呼称" | "別名" | "通称" | "あだ名" | "渾名" | "二つ名"
            | "愛称" | "呼び名" | "異名" => Ok(Self::Alias),
            _ => Err(format!("No such attribute {}", s)),
        }
    }
}

impl CharacterAttribute {
    /// 新規セクション/ファイル作成時に使う日本語正規見出しを返す。
    pub(crate) fn canonical_heading(&self) -> &'static str {
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
pub(crate) struct CharacterCache(pub(crate) Arc<parking_lot::Mutex<HashMap<PathBuf, FileCacheEntry>>>);

impl CharacterCache {
    pub(crate) fn new() -> Self {
        Self(Arc::new(parking_lot::Mutex::new(HashMap::new())))
    }
}

/// workspace 内でキャラクター名に対応するファイルパスを解決する自由関数。
pub(crate) fn find_character_file_path(
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
pub(crate) fn comrak_options() -> comrak::Options<'static> {
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
pub(crate) fn parse_all_content(content: &str) -> HashMap<String, CharacterEntry> {
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
                    let entry =
                        characters
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
pub(crate) fn split_aliases(text: &str) -> Vec<String> {
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
    let end = heading_key.find(['（', '(']).unwrap_or(heading_key.len());
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
            for alias in &file_entry.characters[heading_key].aliases {
                add_name(alias);
                for part in alias.split('・') {
                    add_name(part);
                }
            }
        }
    }

    names
}

/// キャラクターエントリを、キャラ情報ツール向けの完全な Markdown ドキュメントへ変換する。
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
pub(crate) fn lookup_character_markdown(cache: &CharacterCache, surface: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CharacterInfoTool;
    use indoc::indoc;

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
        // LLM 抽出スキーマの enum 値("aliases")も受理されること
        assert_eq!(
            CharacterAttribute::try_from("aliases"),
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
        let md =
            lookup_character_markdown(&cache, "ジェフ・クライン").expect("表示名で見つかるはず");
        assert!(md.contains("予備役"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_split_part() {
        let cache = CharacterCache::new();
        cache
            .0
            .lock()
            .insert(PathBuf::from("characters.md"), make_entry(CHARACTERS_MD));
        let md =
            lookup_character_markdown(&cache, "クライン").expect("「・」分割部分名で見つかるはず");
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

        let md =
            lookup_character_markdown(&cache, "タナカ").expect("両ファイルのタナカが見つかるはず");
        assert!(md.contains("A船の整備士"), "{}", md);
        assert!(md.contains("B船の通信士"), "{}", md);
        assert!(md.contains("---"), "{} に区切り線が含まれるはず", md);
    }
}
