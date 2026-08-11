//! キャラクター設定ファイル(`characters.md` 等)のドメインモデルと Markdown パーサ。
//!
//! `main.rs` から切り出したモジュール。`Backend`(LSP ハンドラ)と
//! `character_updater`(自動更新)の双方から参照される、キャラクター情報の
//! 「読み取り」側を担う。書き込み・マージロジックは `character_updater` 側にある。

use crate::llm::LlmError;
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, options};
#[allow(unused_imports)]
use log::{debug, trace};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::instrument;

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

    #[instrument]
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
    #[instrument]
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

/// 1キャラクターファイルのメモリ上の正本。ディスクはこの値のload/dump先でしかない。
#[derive(Debug, Clone)]
pub(crate) struct CharacterFile {
    /// 現在のMarkdown全文。書き込み(character_updater)・外部変更取り込みの両方でここが更新される。
    pub(crate) content: String,
    /// `content` から派生した読み取り用インデックス(ハイライト許可名・hover検索用)。
    /// `content` を更新するたびに再計算する。
    pub(crate) characters: HashMap<String, CharacterEntry>,
    /// 直前にこのプロセス自身が書き込んだ内容のハッシュ。watcherイベントの内容ハッシュと
    /// 一致すれば自己書き込みのエコーとして無視する(`CharacterStore::reconcile` が使う)。
    pub(crate) last_written_hash: Option<u64>,
}

impl CharacterFile {
    #[instrument]
    fn from_content(content: String) -> Self {
        let characters = parse_all_content(&content);
        Self {
            content,
            characters,
            last_written_hash: None,
        }
    }
}

#[instrument]
fn hash_content(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[instrument]
fn add_allowed_name(n: &str, names: &mut std::collections::HashSet<String>) {
    let n = n.trim();
    if !n.is_empty() {
        names.insert(n.to_string());
    }
}

/// 「・」による複合名分割は行わない(西欧式の名・姓区切りとは限らず、分割するなら
/// 姓・名の区分を登録時に別途持つ必要があるため)。文字数による除外も行わない
/// (1文字の姓等も対象)。誤マッチ抑止はユーザー辞書側のコスト調整
/// (`Highlighter::user_dict_csv_row`)で担保する。
#[instrument]
fn collect_names_from(
    characters: &HashMap<String, CharacterEntry>,
    names: &mut std::collections::HashSet<String>,
) {
    for (heading_key, entry) in characters {
        add_allowed_name(character_display_name(heading_key), names);
        for alias in &entry.aliases {
            add_allowed_name(alias, names);
        }
    }
}

#[instrument]
fn matches_surface(heading_key: &str, entry: &CharacterEntry, surface: &str) -> bool {
    character_display_name(heading_key) == surface || entry.aliases.iter().any(|a| a == surface)
}

#[derive(Debug)]
struct CharacterStoreState {
    /// ワークスペースroot -> そのワークスペース配下のキャラクターファイル群
    workspaces: parking_lot::Mutex<HashMap<PathBuf, HashMap<PathBuf, CharacterFile>>>,
    /// ワークスペースroot単位の書き込み直列化ロック。`character_updater::run` がどの
    /// ドキュメントURIから発火しても、同一ワークスペースの plan→バッチマージ→apply は
    /// このロックで直列化される(同一キャラクターファイルへの並行read-modify-writeを防ぐ)。
    write_locks: dashmap::DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>,
}

/// 全ワークスペースのキャラクター設定ファイルをメモリ上に保持する正本(SSoT)。
/// ディスクはload/dump先でしかない(`Backend`と`CharacterInfoTool`で共有する)。
#[derive(Debug, Clone)]
pub(crate) struct CharacterStore(Arc<CharacterStoreState>);

impl CharacterStore {
    pub(crate) fn new() -> Self {
        Self(Arc::new(CharacterStoreState {
            workspaces: parking_lot::Mutex::new(HashMap::new()),
            write_locks: dashmap::DashMap::new(),
        }))
    }

    /// `root`配下のキャラクターファイル一覧を検出する(`characters.md`単一ファイル形式・
    /// `characters/*.md`フォルダ形式の両対応)。`Backend`/`character_updater`どちらからも
    /// 使う唯一のファイル探索実装。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) fn discover_character_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();

        let single = root.join("characters").with_extension("md");
        if single.is_file() {
            files.push(single);
        }

        let dir = root.join("characters");
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

    /// `root`配下のキャラクターファイルを列挙・読込・パースしてメモリへロードする。
    /// 1件も見つからない場合は空の`characters.md`を新規作成してロードする。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) async fn load_workspace(&self, root: &Path) {
        let mut files = Self::discover_character_files(root);
        if files.is_empty() {
            let new_path = root.join("characters.md");
            match tokio::fs::write(&new_path, "").await {
                Ok(_) => {
                    debug!("CharacterStore::load_workspace: created {:?}", new_path);
                    files.push(new_path);
                }
                Err(e) => {
                    debug!(
                        "CharacterStore::load_workspace: characters.md新規作成に失敗 {:?}: {}",
                        new_path, e
                    );
                    return;
                }
            }
        }

        let mut loaded = HashMap::new();
        for path in files {
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    loaded.insert(path, CharacterFile::from_content(content));
                }
                Err(e) => debug!(
                    "CharacterStore::load_workspace: 読み込み失敗 {:?}: {}",
                    path, e
                ),
            }
        }
        self.0.workspaces.lock().insert(root.to_path_buf(), loaded);
    }

    /// ドキュメントパスを含む最長一致のワークスペースrootを返す。
    #[instrument]
    pub(crate) fn resolve_workspace_for<'a>(
        doc_path: &Path,
        roots: &'a [PathBuf],
    ) -> Option<&'a PathBuf> {
        roots
            .iter()
            .filter(|root| doc_path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len())
    }

    /// 指定ワークスペースの許可名集合を構築する(人名ハイライトの絞り込み用)。
    #[instrument]
    pub(crate) fn allowed_names(&self, workspace_root: &Path) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        let guard = self.0.workspaces.lock();
        if let Some(files) = guard.get(workspace_root) {
            for file in files.values() {
                collect_names_from(&file.characters, &mut names);
            }
        }
        names
    }

    /// 全ワークスペースの許可名の和集合(Linderaユーザー辞書構築用。トークナイズ品質の
    /// 担保だけが目的で、どのワークスペースの名前かを区別する必要はない)。
    #[instrument]
    pub(crate) fn all_allowed_names(&self) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        let guard = self.0.workspaces.lock();
        for files in guard.values() {
            for file in files.values() {
                collect_names_from(&file.characters, &mut names);
            }
        }
        names
    }

    /// 指定ワークスペース内のみを対象にした、`surface`(表示名/alias)に一致する
    /// キャラクターの Markdown 化した説明文を返す(hover表示用)。
    /// 同名のキャラが複数ファイルに存在する場合は "---" 区切りで連結して返す。
    #[instrument]
    pub(crate) fn lookup_markdown(&self, workspace_root: &Path, surface: &str) -> Option<String> {
        if surface.is_empty() {
            return None;
        }
        let guard = self.0.workspaces.lock();
        let files = guard.get(workspace_root)?;
        let hits: Vec<String> = files
            .values()
            .flat_map(|f| f.characters.iter())
            .filter(|(heading_key, entry)| matches_surface(heading_key, entry, surface))
            .map(|(heading_key, entry)| character_entry_to_markdown(heading_key, entry))
            .collect();
        if hits.is_empty() {
            None
        } else {
            Some(hits.join("\n\n---\n\n"))
        }
    }

    /// `name`(部分一致)にマッチする最初のキャラクターについて、`tags`が示す属性の
    /// セクション本文を返す(`CharacterInfoTool`用)。ワークスペース内の全ファイルを
    /// 横断して検索する(1ファイルへの決め打ちをしない)。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) fn search(
        &self,
        workspace_root: &Path,
        name: &str,
        tags: &[CharacterAttribute],
    ) -> std::result::Result<String, LlmError> {
        let guard = self.0.workspaces.lock();
        let Some(files) = guard.get(workspace_root) else {
            return Err(LlmError::GenericError {
                message: format!(
                    "No character files loaded for workspace {:?}",
                    workspace_root
                ),
            });
        };
        for file in files.values() {
            let Some((_, entry)) = file.characters.iter().find(|(k, _)| k.contains(name)) else {
                continue;
            };
            let matched: Vec<&str> = entry
                .sections
                .iter()
                .filter(|s| tags.iter().any(|t| s.tags.contains(t)))
                .map(|s| s.text.trim())
                .filter(|t| !t.is_empty())
                .collect();
            if !matched.is_empty() {
                return Ok(matched.join("\n\n"));
            }
        }
        Err(LlmError::GenericError {
            message: format!("Character '{}' not found (or no matching sections)", name),
        })
    }

    /// メモリの`content`/`characters`/`last_written_hash`を即座に更新してからディスクへ
    /// 書き出す。メモリ更新はディスク書き込みの成否と独立(メモリが正本、ディスクは
    /// ベストエフォートな永続化という設計上の帰結)。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) async fn write(
        &self,
        workspace_root: &Path,
        path: &Path,
        new_content: String,
    ) -> std::io::Result<()> {
        let hash = hash_content(&new_content);
        {
            let mut guard = self.0.workspaces.lock();
            let files = guard.entry(workspace_root.to_path_buf()).or_default();
            let mut file = CharacterFile::from_content(new_content.clone());
            file.last_written_hash = Some(hash);
            files.insert(path.to_path_buf(), file);
        }
        tokio::fs::write(path, new_content).await
    }

    /// `did_save`/`did_change_watched_files`から呼ぶ。`disk_content`のハッシュが
    /// 直前の自己書き込みハッシュと一致すればエコーとして無視し`false`を返す。
    /// 不一致なら真の外部変更としてメモリを全置換し`true`を返す
    /// (呼び出し側は`true`のときだけ`refresh_highlight_names`等の後続処理をする)。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) fn reconcile(
        &self,
        workspace_root: &Path,
        path: &Path,
        disk_content: String,
    ) -> bool {
        let hash = hash_content(&disk_content);
        let mut guard = self.0.workspaces.lock();
        let files = guard.entry(workspace_root.to_path_buf()).or_default();
        if let Some(existing) = files.get(path)
            && existing.last_written_hash == Some(hash)
        {
            return false;
        }
        files.insert(
            path.to_path_buf(),
            CharacterFile::from_content(disk_content),
        );
        true
    }

    /// 指定ワークスペースの該当ファイルをメモリから除去する(削除イベント用)。
    #[instrument]
    pub(crate) fn remove(&self, workspace_root: &Path, path: &Path) {
        if let Some(files) = self.0.workspaces.lock().get_mut(workspace_root) {
            files.remove(path);
        }
    }

    /// 指定ワークスペースのキャラクターファイルパス一覧を返す。
    #[instrument]
    pub(crate) fn files_in(&self, workspace_root: &Path) -> Vec<PathBuf> {
        self.0
            .workspaces
            .lock()
            .get(workspace_root)
            .map(|f| f.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// 指定ワークスペースの、指定パスの現在のMarkdown全文を返す。
    #[instrument]
    pub(crate) fn content_of(&self, workspace_root: &Path, path: &Path) -> Option<String> {
        self.0
            .workspaces
            .lock()
            .get(workspace_root)?
            .get(path)
            .map(|f| f.content.clone())
    }

    /// ワークスペースroot単位の書き込みロックを取得する。`character_updater::run`が
    /// どのドキュメントURIから発火しても、同一ワークスペースの plan→バッチマージ→apply
    /// はこのガードが生きている間、直列化される。
    #[cfg_attr(feature = "otel", tracing::instrument())]
    pub(crate) async fn acquire_write_lock(
        &self,
        workspace_root: &Path,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .0
            .write_locks
            .entry(workspace_root.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }
}

/// ファイル内の heading 構造からキャラクターを表す heading レベルを推定する。
///
/// 各 heading レベルの出現回数と「直後に level+1 の heading が続くか」を調べ、
/// 最も出現回数が多い「子持ちレベル」を返す。タイ時は深レベル(より多くの # を持つ)優先。
/// 例: `# Story / ## キャラ / ### 属性` のように各レベルが 1 件ずつの場合、
/// タイトル(1)よりキャラクター(2)を選ぶ方が意味的に正しい。
#[instrument]
pub(crate) fn detect_char_level<'a>(root: &'a AstNode<'a>) -> u8 {
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
#[instrument]
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

/// Markdown 文字列をパースし、全キャラクターの全セクションを `HashMap` で返す。
///
/// キーは heading 全文（例: "ジェフ・クライン（艦長）"）。
#[cfg_attr(feature = "otel", tracing::instrument())]
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
                    debug!("{}", t);
                    current_char = Some(t);
                } else {
                    // タイトルなどキャラクターレベルより上の heading はスキップ
                    current_char = None;
                }
            }
            NodeValue::Heading(h) if h.level == char_level + 1 && current_char.is_some() => {
                flush_section!();
                let t = heading_text(node);
                debug!("{}", t);
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
#[instrument]
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
#[instrument]
fn character_display_name(heading_key: &str) -> &str {
    let end = heading_key.find(['（', '(']).unwrap_or(heading_key.len());
    heading_key[..end].trim()
}

/// キャラクターエントリを、キャラ情報ツール向けの完全な Markdown ドキュメントへ変換する。
#[instrument]
fn character_entry_to_markdown(heading_key: &str, entry: &CharacterEntry) -> String {
    let mut out = format!("# {}", heading_key);

    for section in &entry.sections {
        let text = section.text.trim();
        if text.is_empty() {
            continue;
        }
        if section.tags.is_empty() {
            // 未知の属性名で分類できなかったセクション。見出しを復元できないので
            // 従来通り本文だけを出す(壊れた入力でパースそのものを失敗させない)。
            out.push_str(&format!("\n\n{}", text));
        } else {
            // 見出しを `canonical_heading()` で復元する。「描写・視点」のように
            // 複数タグが同じ属性へマップされることがあるため、出現順を保ったまま
            // 重複除去してから「・」で結合する。
            let mut seen: Vec<&CharacterAttribute> = Vec::new();
            for t in &section.tags {
                if !seen.contains(&t) {
                    seen.push(t);
                }
            }
            let heading = seen
                .iter()
                .map(|t| t.canonical_heading())
                .collect::<Vec<_>>()
                .join("・");
            out.push_str(&format!("\n\n## {}\n{}", heading, text));
        }
    }

    out
}

/// 見出しノードの直接子から `Text` ノードを結合してキャラクター名や属性名を返す。
#[instrument]
pub(crate) fn heading_text<'a>(node: &'a AstNode<'a>) -> String {
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
#[instrument]
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

    /// テスト用: 単一ワークスペース("/ws")に`files`(ファイル名, Markdown全文)を
    /// 全て読み込んだ`CharacterStore`を作る。
    fn make_store(files: &[(&str, &str)]) -> (CharacterStore, PathBuf) {
        let store = CharacterStore::new();
        let root = PathBuf::from("/ws");
        for (name, md) in files {
            store.reconcile(&root, &root.join(name), md.to_string());
        }
        (store, root)
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
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let result = store.search(&root, "クライン", &[CharacterAttribute::Background]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().contains("予備役"));
    }

    #[test]
    fn test_parse_characters_md_expression() {
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let result = store.search(&root, "シルビア", &[CharacterAttribute::Style]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().contains("成長の兆し"));
    }

    #[test]
    fn test_parse_characters_md_personality() {
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let result = store.search(&root, "ジェフ", &[CharacterAttribute::Personality]);
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().starts_with("落ち着いていて"));
    }

    #[test]
    fn test_parse_characters_md_multi_tag_from_heading() {
        // "性格・口調" heading が ["性格", "口調"] に分割されること
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let by_kuchou = store.search(&root, "ジェフ", &[CharacterAttribute::Expression]);
        assert!(
            by_kuchou.is_ok(),
            "「口調」タグでヒットしない: {:?}",
            by_kuchou
        );
        let by_seikaku = store.search(&root, "ジェフ", &[CharacterAttribute::Personality]);
        assert_eq!(
            by_kuchou.unwrap(),
            by_seikaku.unwrap(),
            "「口調」と「性格」は同じセクションを返すはず"
        );
    }

    #[test]
    fn test_parse_characters_md_or_search() {
        // 複数タグ OR 検索: 異なるセクションがまとめて返ること
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let result = store.search(
            &root,
            "クライン",
            &[
                CharacterAttribute::Background,
                CharacterAttribute::Personality,
            ],
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
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let result = store.search(&root, "ユルゲン", &[CharacterAttribute::Personality]);
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

    /// `detect_char_level` のテスト用ヘルパ: Markdown文字列をパースしてから渡す。
    fn detect_char_level_str(text: &str) -> u8 {
        let arena = Arena::new();
        let options = comrak_options();
        let root = comrak::parse_document(&arena, text, &options);
        detect_char_level(root)
    }

    #[test]
    fn test_detect_char_level_str_with_story_heading() {
        // # Story / ## キャラ / ### 属性 の構造
        let md = "\
# Story
## ジェフ
### 性格
- 落ち着いている。
## シルビア
### 背景
- 不明。
";
        assert_eq!(detect_char_level_str(md), 2);
    }

    #[test]
    fn test_detect_char_level_str_top_level_chars() {
        // # キャラ / ## 属性 の構造
        let md = "\
# ジェフ
## 性格
- 落ち着いている。
# シルビア
## 背景
- 不明。
";
        assert_eq!(detect_char_level_str(md), 1);
    }

    #[test]
    fn test_detect_char_level_str_empty() {
        assert_eq!(detect_char_level_str(""), 0);
        assert_eq!(detect_char_level_str("本文だけ、見出しなし。"), 0);
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
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let names = store.allowed_names(&root);

        // フルネーム(表示名)がそのまま含まれること。「・」による分割は行わない
        assert!(names.contains("ジェフ・クライン"), "{:?}", names);
        assert!(!names.contains("ジェフ"), "{:?}", names);
        assert!(!names.contains("クライン"), "{:?}", names);
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
        let (store, root) = make_store(&[("characters.md", MD)]);
        let names = store.allowed_names(&root);
        assert!(names.contains("隊長"), "{:?}", names);
    }

    #[test]
    fn test_collect_allowed_names_includes_single_char_alias() {
        const MD: &str = indoc!(
            "## 原顕三郎（司令）
            ### 呼称
            - 原
            "
        );
        let (store, root) = make_store(&[("characters.md", MD)]);
        let names = store.allowed_names(&root);
        // 1文字のaliasも文字数で除外されず許可名に含まれること
        assert!(names.contains("原"), "{:?}", names);
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
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        let md = store
            .lookup_markdown(&root, "ジェフ・クライン")
            .expect("表示名で見つかるはず");
        assert!(md.contains("予備役"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_does_not_match_partial_name() {
        // 「・」による複合名分割は行わないため、部分名では見つからないこと
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        assert!(store.lookup_markdown(&root, "クライン").is_none());
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
        let (store, root) = make_store(&[("characters.md", MD)]);
        let md = store
            .lookup_markdown(&root, "隊長")
            .expect("aliasで見つかるはず");
        assert!(md.contains("艦長"), "{}", md);
    }

    #[test]
    fn test_lookup_character_markdown_miss() {
        let (store, root) = make_store(&[("characters.md", CHARACTERS_MD)]);
        assert!(store.lookup_markdown(&root, "存在しない人").is_none());
        // どの表示名/aliasとも完全一致しないため None
        assert!(store.lookup_markdown(&root, "ジ").is_none());
    }

    #[test]
    fn test_lookup_character_markdown_single_char_alias() {
        const MD: &str = indoc!(
            "## 原顕三郎（司令）
            ### 呼称
            - 原
            ### 背景・立場
            - 遣泰艦隊司令。
            "
        );
        let (store, root) = make_store(&[("characters.md", MD)]);
        let md = store
            .lookup_markdown(&root, "原")
            .expect("1文字aliasで見つかるはず");
        assert!(md.contains("遣泰艦隊司令"), "{}", md);
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
        let (store, root) = make_store(&[("a.md", MD_A), ("b.md", MD_B)]);

        let md_a = store
            .lookup_markdown(&root, "アリス")
            .expect("a.mdのキャラが見つかるはず");
        assert!(md_a.contains("整備班"), "{}", md_a);
        let md_b = store
            .lookup_markdown(&root, "ボブ")
            .expect("b.mdのキャラが見つかるはず");
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
        let (store, root) = make_store(&[("a.md", MD_A), ("b.md", MD_B)]);

        let md = store
            .lookup_markdown(&root, "タナカ")
            .expect("両ファイルのタナカが見つかるはず");
        assert!(md.contains("A船の整備士"), "{}", md);
        assert!(md.contains("B船の通信士"), "{}", md);
        assert!(md.contains("---"), "{} に区切り線が含まれるはず", md);
    }

    #[tokio::test]
    async fn test_reconcile_ignores_self_write_echo() {
        // write() で書いた内容と同じ内容のreconcileは自己書き込みのエコーとしてfalseを返す
        // (再パース不要・変更なし)。異なる内容なら真の外部変更としてtrueを返す。
        let dir = std::env::temp_dir().join("ff_character_store_echo_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("characters.md");

        let store = CharacterStore::new();
        store
            .write(&dir, &path, CHARACTERS_MD.to_string())
            .await
            .unwrap();

        assert!(
            !store.reconcile(&dir, &path, CHARACTERS_MD.to_string()),
            "自己書き込みと同一内容はエコーとして無視されるはず"
        );

        let changed = format!("{}\n追記。", CHARACTERS_MD);
        assert!(
            store.reconcile(&dir, &path, changed.clone()),
            "内容が異なれば真の外部変更として取り込まれるはず"
        );
        assert_eq!(store.content_of(&dir, &path), Some(changed));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_workspace_for_picks_longest_match() {
        let roots = vec![PathBuf::from("/ws"), PathBuf::from("/ws/nested")];
        let doc = PathBuf::from("/ws/nested/chapters/ch01.md");
        assert_eq!(
            CharacterStore::resolve_workspace_for(&doc, &roots),
            Some(&roots[1])
        );
    }

    #[test]
    fn test_resolve_workspace_for_no_match_returns_none() {
        let roots = vec![PathBuf::from("/ws")];
        let doc = PathBuf::from("/other/chapters/ch01.md");
        assert_eq!(CharacterStore::resolve_workspace_for(&doc, &roots), None);
    }
}
