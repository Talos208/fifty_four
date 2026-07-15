use crate::llm::{Content, LlmInterface, ModelCapability};
use crate::{CharacterAttribute, FlightRecorder, parse_all_content, shorten_middle, split_aliases};
use dashmap::DashMap;
use genai::chat::{ChatResponseFormat, JsonSpec};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

use crate::types::LineData;

pub const DEFAULT_MIN_CHARS: usize = 1000;
pub const DEFAULT_MAX_CHARS: usize = 5000;
pub const DEFAULT_IDLE_SECS: u64 = 3 * 60;

/// URI ごとの更新トリガー状態
#[derive(Debug)]
pub struct UpdateState {
    pub last_change_at: Instant,
    pub first_dirty_at: Option<Instant>,
    pub accumulated_chars: usize,
    /// `run` タスクが実行中かどうか。実行中はカウントのみ行い発火判定をスキップする。
    pub running: bool,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            last_change_at: Instant::now(),
            first_dirty_at: None,
            accumulated_chars: 0,
            running: false,
        }
    }
}

impl UpdateState {
    /// 現在のバーストのカウントをクリアする。`running` は触らない。
    pub fn reset(&mut self) {
        self.accumulated_chars = 0;
        self.first_dirty_at = None;
    }
}

/// 発火判定の結果。
#[derive(Debug, PartialEq)]
pub enum Trigger {
    /// まだ待機(直前バーストは継続中、または何も溜まっていない)。
    None,
    /// 直前バーストが idle 確定し、min_chars 以上なので発火する。
    Fire,
    /// 直前バーストが idle 確定したが min_chars 未満なので破棄する。
    ClearStale,
}

/// 新しい変更を取り込む *前* に、直前バーストの idle 判定を行う。
/// `gap` は前回変更からの経過時間。
pub fn idle_trigger(
    accumulated: usize,
    gap: Duration,
    idle_timeout: Duration,
    min_chars: usize,
) -> Trigger {
    if accumulated == 0 || gap < idle_timeout {
        return Trigger::None;
    }
    if accumulated >= min_chars {
        Trigger::Fire
    } else {
        Trigger::ClearStale
    }
}

/// 指定 URI の全行を "\n" で連結して全文テキストを返す。
/// LLM に渡す本文テキストとして使用する(発火判定の `accumulated_chars` とは独立)。
pub fn full_text(text: &DashMap<String, Vec<LineData>>, uri: &str) -> String {
    text.get(uri)
        .map(|lines| {
            lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// スキーマ準拠に正規化した1件の更新項目。
#[derive(Debug, PartialEq)]
pub struct UpdateItem {
    pub name: String,
    pub attribute: String,
    pub text: String,
}

/// LLM 応答からコードフェンスや前後の説明文を除いた JSON 部分を取り出す。
fn extract_json(response: &str) -> Option<&str> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    (start <= end).then(|| &response[start..=end])
}

/// JSON 値をセクション本文向けの可読文字列へフラット化する。
/// `text` に文字列以外(オブジェクト/配列)を返すスキーマ違反応答の救済に使う。
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(items) => items
            .iter()
            .map(value_to_text)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let body = value_to_text(v);
                if body.contains('\n') {
                    format!("{}:\n{}", k, body)
                } else {
                    format!("{}: {}", k, body)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// フラット形式 `{name, attribute, text}` の1項目を正規化する。
/// `text` が文字列以外の場合はフラット化して救済する。
fn update_from_flat(item: &Value) -> Option<UpdateItem> {
    let name = item.get("name").and_then(|v| v.as_str())?.trim().to_string();
    let attribute = item
        .get("attribute")
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    let text = value_to_text(item.get("text")?);
    if name.is_empty() || text.trim().is_empty() {
        return None;
    }
    Some(UpdateItem {
        name,
        attribute,
        text,
    })
}

/// `{"name": ..., "personality": ..., "expression": {...}}` のような
/// キャラクター単位のネスト形式1件をフラットな更新項目列へ救済変換する。
/// `CharacterAttribute` として解釈できるキーのみ拾う。
fn updates_from_character(item: &Value) -> Vec<UpdateItem> {
    let Some(obj) = item.as_object() else {
        return Vec::new();
    };
    let Some(name) = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(k, _)| k.as_str() != "name")
        .filter(|(k, _)| CharacterAttribute::try_from(k.as_str()).is_ok())
        .filter_map(|(k, v)| {
            let text = value_to_text(v);
            (!text.trim().is_empty()).then(|| UpdateItem {
                name: name.to_string(),
                attribute: k.clone(),
                text,
            })
        })
        .collect()
}

/// LLM レスポンスをフラットな `UpdateItem` 列へ正規化する。
/// - 正規形式 `{"updates": [{name, attribute, text}]}` を受理
/// - `{"characters": [...]}` のようなキャラクター単位のネスト形式も救済変換
/// - どちらとしても解釈できない場合は `None`(呼び出し側でリトライ)
fn parse_updates(response: &str) -> Option<Vec<UpdateItem>> {
    let json = extract_json(response)?;
    let parsed: Value = serde_json::from_str(json).ok()?;

    if let Some(arr) = parsed.get("updates").and_then(|v| v.as_array()) {
        return Some(arr.iter().filter_map(update_from_flat).collect());
    }

    if let Some(arr) = parsed.get("characters").and_then(|v| v.as_array()) {
        let items: Vec<UpdateItem> = arr.iter().flat_map(updates_from_character).collect();
        // 救済変換で1件も取れなければ不正応答としてリトライに回す
        return (!items.is_empty()).then_some(items);
    }

    None
}

/// Markdown ファイルのテキストを行走査して、指定キャラクター・属性のセクション本文を差し替える。
/// 対象が見つかった場合は新しいファイル全体のテキストを返し、見つからなければ None を返す。
pub fn replace_section(
    file_text: &str,
    char_name: &str,
    attribute: &CharacterAttribute,
    new_body: &str,
) -> Option<String> {
    let lines: Vec<&str> = file_text.lines().collect();
    let total = lines.len();

    // heading の "#" 数と見出しテキストを返す(owned String で返してライフタイム問題を回避)
    let parse_heading = |line: &str, in_fence: bool| -> Option<(u8, String)> {
        if in_fence {
            return None;
        }
        let trimmed = line.trim_end();
        if !trimmed.starts_with('#') {
            return None;
        }
        let level = trimmed.bytes().take_while(|&b| b == b'#').count() as u8;
        let text = trimmed[level as usize..].trim().to_string();
        Some((level, text))
    };

    let mut in_fence = false;
    let mut char_level: Option<u8> = None;
    let mut char_start: Option<usize> = None;
    let mut attr_body_start: Option<usize> = None;
    let mut attr_body_end: Option<usize> = None;

    for (i, &line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }

        if let Some((level, text)) = parse_heading(line, in_fence) {
            if let Some(cl) = char_level {
                if level <= cl {
                    // キャラクターレベルより上か同レベルの heading に来た → キャラクター終了
                    if attr_body_start.is_some() && attr_body_end.is_none() {
                        attr_body_end = Some(i);
                        break;
                    }
                    char_level = None;
                    char_start = None;
                    attr_body_start = None;
                }
                if let Some(astart) = attr_body_start {
                    if attr_body_end.is_none() && level <= cl + 1 {
                        // 属性ヘッダより同レベル/上位の heading → セクション終了
                        attr_body_end = Some(i);
                        break;
                    }
                    let _ = astart;
                }
                if attr_body_start.is_none() && level == cl + 1 {
                    // 属性ヘッダ候補
                    let attr_tags: Vec<CharacterAttribute> = text
                        .split(['・', '、', ',', '/', ' '])
                        .filter_map(|s| CharacterAttribute::try_from(s).ok())
                        .collect();
                    if attr_tags.contains(attribute) {
                        attr_body_start = Some(i + 1);
                    }
                }
            } else {
                // キャラクターヘッダを探す
                if text.contains(char_name) {
                    char_level = Some(level);
                    char_start = Some(i);
                }
            }
        }
    }

    // ファイル末尾でセクション終了した場合
    if attr_body_start.is_some() && attr_body_end.is_none() {
        attr_body_end = Some(total);
    }

    let (body_start, body_end) = match (attr_body_start, attr_body_end) {
        (Some(s), Some(e)) => (s, e),
        _ => return None,
    };

    let _ = char_start;

    let mut out = String::new();
    for (i, &line) in lines.iter().enumerate() {
        if i < body_start || i >= body_end {
            out.push_str(line);
            out.push('\n');
        } else if i == body_start {
            if !new_body.is_empty() {
                out.push_str(new_body);
                if !new_body.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }

    Some(out)
}

/// 書き込み直前に Markdown 全体を comrak で正規化する(パース→CommonMark 書き戻し)。
/// パースと同じ `comrak_options` を使い、折返しなし(width=0)・箇条書き "-" で出力する。
/// セクション差し替え・追記の繰り返しで生じる空行や記法の不揃いを、書き込みのたびに整える。
fn format_markdown(text: &str) -> String {
    let mut options = crate::comrak_options();
    // インデント型ではなくフェンス型でコードブロックを保持する
    // (replace_section 等の行走査が ``` フェンスを前提にしているため)。
    options.render.prefer_fenced = true;
    comrak::markdown_to_commonmark(text, &options)
}

/// `CharacterAttribute` の JSON スキーマ上の英語 enum 値を返す。
/// `strip_attribute_label` でラベル判定に使う(canonical_heading と合わせて2通り許容)。
fn attr_schema_key(attr: &CharacterAttribute) -> &'static str {
    match attr {
        CharacterAttribute::Appearance => "appearance",
        CharacterAttribute::Background => "background",
        CharacterAttribute::Expression => "expression",
        CharacterAttribute::Personality => "personality",
        CharacterAttribute::Relationship => "relationship",
        CharacterAttribute::Role => "role",
        CharacterAttribute::Style => "style",
        CharacterAttribute::Weakness => "weakness",
        CharacterAttribute::Alias => "aliases",
    }
}

/// 1行の先頭から「<ラベル>：」または「<ラベル>:」を取り除く。
/// ラベルが `canonical`(日本語見出し)または `english_key`(スキーマ enum 値)と
/// 完全一致する場合のみ除去し、それ以外(「一人称：」等の情報を持つサブラベル)は素通しする。
fn strip_label_from_line<'a>(line: &'a str, canonical: &str, english_key: &str) -> Option<&'a str> {
    for sep in ['：', ':'] {
        if let Some((label, body)) = line.split_once(sep) {
            let label = label.trim();
            if label == canonical || label.eq_ignore_ascii_case(english_key) {
                return Some(body);
            }
        }
    }
    None
}

/// 抽出/マージ LLM が誤って `text` の先頭に付与した属性ラベル(「口調：」「呼称：」等)を取り除く。
/// 見出し(`## 口調`)の下にラベルが重複して書かれてしまう問題への対処。
/// ラベル行が空になった場合(「呼称：\n- 飛騨艦長」のような形)は、その行ごと取り除く。
fn strip_attribute_label(text: &str, attr: &CharacterAttribute) -> String {
    let canonical = attr.canonical_heading();
    let english_key = attr_schema_key(attr);

    let trimmed = text.trim_start();
    match trimmed.split_once('\n') {
        Some((first_line, rest)) => match strip_label_from_line(first_line, canonical, english_key) {
            Some(stripped) => {
                let stripped = stripped.trim_start();
                if stripped.is_empty() {
                    rest.trim_start().to_string()
                } else {
                    format!("{}\n{}", stripped, rest)
                }
            }
            None => text.to_string(),
        },
        None => match strip_label_from_line(trimmed, canonical, english_key) {
            Some(stripped) => stripped.trim_start().to_string(),
            None => text.to_string(),
        },
    }
}

/// Alias(呼称)属性専用の決定的マージ。呼称は名前の列挙であり自由記述ではないため、
/// LLM による意味マージ(1行の文章に統合されてしまう)ではなく `split_aliases` で
/// 分割した別名リストを箇条書きとして順序保持のまま結合する。
/// `char_name` 自身と一致する別名は除外する。新規に追加される別名が無ければ `None`。
///
/// `split_aliases` は「：」「:」を分割文字に含まないため、過去のバグで
/// 「呼称：飛騨艦長」のように属性ラベルが1トークンに混入したまま保存された旧データが
/// 残っている場合がある。各トークンに `strip_attribute_label` を適用してから比較することで、
/// 新しく来た清潔な「飛騨艦長」と同一別名として認識し、重複を防ぎつつ自己修復する。
fn merge_alias_bodies(old_body: &str, new_text: &str, char_name: &str) -> Option<String> {
    let dedup_excluding_self = |raw: Vec<String>| -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        raw.into_iter()
            .map(|a| strip_attribute_label(&a, &CharacterAttribute::Alias))
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty() && a != char_name)
            .filter(|a| seen.insert(a.clone()))
            .collect()
    };

    let old_list = dedup_excluding_self(split_aliases(old_body));
    let new_list = dedup_excluding_self(split_aliases(new_text));

    let mut combined = old_list.clone();
    for alias in new_list {
        if !combined.contains(&alias) {
            combined.push(alias);
        }
    }

    if combined == old_list {
        return None;
    }

    Some(
        combined
            .iter()
            .map(|a| format!("- {}", a))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 新規セクション/新規キャラクターの本文を組み立てる。
/// Alias は箇条書きへ正規化し(`merge_alias_bodies` を空の旧本文で流用)、
/// 追加すべき別名が無ければ `None`(呼び出し側で "no change" として記録・スキップする)。
fn build_new_section_body(attr: &CharacterAttribute, new_text: &str, char_name: &str) -> Option<String> {
    if *attr == CharacterAttribute::Alias {
        merge_alias_bodies("", new_text, char_name)
    } else {
        Some(new_text.to_string())
    }
}

fn build_semantic_merge_prompt(
    char_name: &str,
    attr_heading: &str,
    old_body: &str,
    new_body: &str,
) -> String {
    format!(
        "\
キャラクター設定の属性セクション本文を、意味が自然につながるようにマージしてください。

# キャラクター
{char_name}

# 属性
{attr_heading}

# 現在の設定
{old_body}

# 新しく本文から読み取れた情報
{new_body}

# 出力ルール
- 出力はマージ後の属性セクション本文のみ。見出し、説明、JSON、コードフェンスは出力しない。
- 行頭に属性名のラベル(「口調：」「呼称：」等)を付けない。見出しで既に示されているため不要。
- 「マージ後の文章：」のような、これから本文を書くことを説明する前置きも一切付けない。1行目からいきなり本文を書き始めること。
  - 悪い例: 「マージ後の文章：\n落ち着いた性格。」
  - 良い例: 「落ち着いた性格。」
- 現在の設定をベースにし、消えていない既存情報は保持する。
- 新情報が既存情報と同じ意味なら重複させず、より自然な一文または箇条書きに統合する。
- 新情報が既存情報と矛盾・更新関係にある場合は、両方を機械的に並べず、矛盾しない表現へ合成する。必要なら「以前は」「現在は」「普段は」「状況によって」などの限定を使う。
- 本文から読み取れない内容を推測で追加しない。
- 既存の Markdown 箇条書きスタイルをできるだけ維持する。
",
        old_body = old_body.trim(),
        new_body = new_body.trim(),
    )
}

fn sanitize_merged_section_text(response: &str) -> String {
    let trimmed = response.trim();
    if let Some(stripped) = trimmed
        .strip_prefix("```markdown")
        .or_else(|| trimmed.strip_prefix("```md"))
        .or_else(|| trimmed.strip_prefix("```"))
    {
        return stripped
            .strip_suffix("```")
            .unwrap_or(stripped)
            .trim()
            .to_string();
    }
    trimmed.to_string()
}

/// マージ LLM が指示に反して付けてしまう「これから本文を書く」旨の前置きラベル
/// (「マージ後の文章：」等)を、先頭行が既知の語のみで構成される場合に限り取り除く。
/// 「地の文から推定：〜」のように本文として要求している内容付きのラベルは対象外
/// (先頭行がラベル語のみ・コロンの後に何も無い場合のみマッチするため誤爆しない)。
fn strip_merge_preamble(text: &str) -> String {
    const PREAMBLE_LABELS: &[&str] = &[
        "マージ後の文章",
        "マージ後の本文",
        "マージ後の内容",
        "マージ後",
        "統合後の文章",
        "統合後の本文",
        "統合後",
        "マージ結果",
        "統合結果",
        "出力",
        "回答",
        "結果",
    ];

    let trimmed = text.trim_start();
    let Some((first_line, rest)) = trimmed.split_once('\n') else {
        return text.to_string();
    };
    let first_line = first_line.trim();
    let label = first_line
        .strip_suffix('：')
        .or_else(|| first_line.strip_suffix(':'))
        .unwrap_or(first_line);

    if PREAMBLE_LABELS.contains(&label) {
        rest.trim_start().to_string()
    } else {
        text.to_string()
    }
}

/// マージ結果の JSON スキーマ(`{"merged_text": "..."}`)。
/// structured output 対応モデルに使わせ、前置き混入を構造的に防ぐ。
fn merge_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "merged_text": { "type": "string" }
        },
        "required": ["merged_text"]
    })
}

/// LLM の生応答からマージ後本文を取り出す。
/// structured output で `{"merged_text": "..."}` が返っていればそれを使い、
/// そうでなければ従来通り生テキストとして扱う(コードフェンス・前置きラベルを除去)。
fn extract_merged_text(raw: &str) -> String {
    if let Some(json_part) = extract_json(raw)
        && let Ok(v) = serde_json::from_str::<Value>(json_part)
        && let Some(s) = v.get("merged_text").and_then(|v| v.as_str())
    {
        return strip_merge_preamble(&sanitize_merged_section_text(s));
    }
    strip_merge_preamble(&sanitize_merged_section_text(raw))
}

async fn merge_section_text_semantically(
    llm_client: &mut dyn LlmInterface,
    char_name: &str,
    attribute: &CharacterAttribute,
    old_body: &str,
    new_body: &str,
) -> Result<Option<String>, crate::llm::LlmError> {
    if new_body.trim().is_empty() {
        return Ok(None);
    }

    let prompt =
        build_semantic_merge_prompt(char_name, attribute.canonical_heading(), old_body, new_body);
    llm_client.temperature(0.2);
    llm_client.max_tokens(2048);
    llm_client.reasoning_level(0.0);
    if llm_client
        .capabilities()
        .contains(ModelCapability::STRUCTURED_OUTPUT)
    {
        llm_client.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
            "merged_section",
            merge_output_schema(),
        )));
    }
    llm_client.add(Content::Text(prompt));
    let merged = extract_merged_text(&llm_client.chat().await?);

    if merged.trim().is_empty() || merged.trim() == old_body.trim() {
        return Ok(None);
    }

    Ok(Some(merged))
}

/// `run` タスクが完了・中断・panic したとき確実に `running = false` に戻す Drop ガード。
struct RunningGuard(Arc<parking_lot::Mutex<UpdateState>>);
impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.lock().running = false;
    }
}

/// キャラクター更新タスクの本体。`did_change` から spawn される唯一の非同期タスク。
/// `full_text` は発火時にスナップショットした編集ファイルの全文テキスト。
/// 完了時は `running = false` に戻すだけで、カウンタはリセットしない
/// (発火時に既にリセット済み・実行中に入った編集を保持するため)。
pub async fn run(
    uri: String,
    workspace_arc: Arc<TokioMutex<Vec<PathBuf>>>,
    full_text: String,
    llm: Arc<TokioMutex<Option<Box<dyn LlmInterface>>>>,
    recorder: Arc<FlightRecorder>,
    state: Arc<parking_lot::Mutex<UpdateState>>,
) {
    // Drop ガード: 正常終了・早期 return・panic いずれの場合も running を false に戻す。
    let _guard = RunningGuard(state.clone());

    debug!(
        "character_updater::run start for {} ({} chars, full text)",
        uri,
        full_text.chars().count()
    );

    if full_text.is_empty() {
        debug!("character_updater::run: full_text empty, abort");
        return;
    }

    let workspace = {
        let ws = workspace_arc.lock().await;
        ws.first().cloned().unwrap_or_default()
    };
    debug!("character_updater::run: workspace={:?}", workspace);

    // 1. characters/*.md を全て収集(1件も無ければ characters.md を新規作成して処理を続ける)
    let mut char_files = collect_character_files(&workspace).await;
    if char_files.is_empty() {
        debug!(
            "character_updater: no character files found under {:?}, creating characters.md",
            workspace
        );
        let new_path = workspace.join("characters.md");
        match tokio::fs::write(&new_path, "").await {
            Ok(_) => {
                info!("character_updater: created {:?}", new_path);
                char_files.push(new_path);
            }
            Err(e) => {
                error!("character_updater: failed to create characters.md: {}", e);
                return;
            }
        }
    }
    debug!(
        "character_updater::run: {} character file(s) found",
        char_files.len()
    );

    // 2. プロンプトを読み込み、frontmatter を分離(補完側と共通のヘルパを使用)
    let (prompt_body, frontmatter_data) = match crate::load_prompt("prompt_character_update.md") {
        Some(v) => v,
        None => {
            error!("prompt_character_update.md not found");
            return;
        }
    };

    let prompt = prompt_body.replace("{{TEXT}}", &full_text);

    // 3. LLM 呼び出し
    let schema_str = frontmatter_data.get("schema").cloned().unwrap_or_default();
    let schema: Value = match serde_json::from_str(&schema_str) {
        Ok(v) => v,
        Err(e) => {
            error!("character_updater: invalid schema: {}", e);
            error!("{}", schema_str);
            return;
        }
    };

    let model_name;
    {
        let mut ref_llm = llm.lock().await;
        let Some(llm_client) = ref_llm.as_mut() else {
            debug!("character_updater: LLM not initialized");
            return;
        };
        model_name = llm_client.get_model().to_string();
        debug!("character_updater::run: calling LLM model={}", model_name);

        let update_id = recorder.record_character_update(&uri, &model_name, &prompt);

        let structured = llm_client
            .capabilities()
            .contains(ModelCapability::STRUCTURED_OUTPUT);
        if structured {
            llm_client.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                "character_updates",
                schema.clone(),
            )));
        } else {
            llm_client.add(Content::Text(format!(
                "回答は次のJSON schemaに厳密にしたがって生成せよ。\n\nJSON Schema:\n{}\n\n最終応答はスキーマに適合するJSONのみを出力し、JSON以外の文字は一切含めないこと。",
                schema_str
            )));
        }
        llm_client.add(Content::Text(prompt.clone()));
        llm_client.reasoning_level(0.8); // 重要なので考える

        match llm_client.chat().await {
            Ok(resp) => {
                debug!("character_updater::run: LLM response received :{}", resp);
                recorder.record_character_response(update_id, &resp);

                // 4. 応答をスキーマ準拠の updates 列へ正規化。
                //    解釈不能な場合は、不正応答を添えて1回だけ修正を要求する。
                let mut updates = parse_updates(&resp);
                if updates.is_none() {
                    warn!("character_updater: response does not conform to schema, retrying once");
                    // chat() がオプションをリセットするため response_format を再設定する
                    if structured {
                        llm_client.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                            "character_updates",
                            schema.clone(),
                        )));
                    }
                    llm_client.reasoning_level(0.8);
                    llm_client.add(Content::Text(format!(
                        "以下はスキーマに適合しない不正な応答である。JSON schemaに厳密に適合するJSONへ修正して出力し直せ。JSON以外の文字は一切含めないこと。\n\nJSON Schema:\n{}\n\n不正な応答:\n{}",
                        schema_str, resp
                    )));
                    match llm_client.chat().await {
                        Ok(retry_resp) => {
                            debug!(
                                "character_updater::run: retry response received :{}",
                                retry_resp
                            );
                            recorder.record_character_response(update_id, &retry_resp);
                            updates = parse_updates(&retry_resp);
                        }
                        Err(e) => error!("character_updater: retry failed: {}", e),
                    }
                }

                // 5. 差し替え・追記・新規作成を適用
                match updates {
                    Some(items) => {
                        apply_updates(
                            &items,
                            update_id,
                            char_files,
                            &workspace,
                            &recorder,
                            llm_client.as_mut(),
                        )
                        .await;
                    }
                    None => {
                        error!("character_updater: could not parse LLM response as updates");
                    }
                }
                recorder.complete_character_update(update_id);
            }
            Err(e) => {
                error!("character_updater: LLM error: {}", e);
                recorder.record_character_response(update_id, &format!("ERROR: {}", e));
            }
        }
    }

    debug!("character_updater::run done for {}", uri);
    // _guard が Drop されて running = false に戻る
}

/// characters/ 配下の全 .md ファイルのパスを収集する。
async fn collect_character_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    let single = workspace.join("characters.md");
    if single.is_file() {
        files.push(single);
        return files;
    }

    let dir = workspace.join("characters");
    if !dir.is_dir() {
        return files;
    }

    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            if p.extension().map(|e| e == "md").unwrap_or(false) {
                files.push(p);
            }
        }
    }

    files
}

/// 正規化済みの更新項目列を characters/*.md に差し替え・追記・新規作成として適用する。
/// `char_files` は値で受け取り、新規作成したファイルを push する
/// (同一レスポンス内の後続 item が同じキャラの別属性を追記できるよう)。
async fn apply_updates(
    updates: &[UpdateItem],
    update_id: i64,
    mut char_files: Vec<PathBuf>,
    workspace: &Path,
    recorder: &FlightRecorder,
    llm_client: &mut dyn LlmInterface,
) {
    debug!("character_updater: {} update(s) received", updates.len());

    for item in updates {
        let name = item.name.as_str();
        let attr_str = item.attribute.as_str();

        debug!(
            "character_updater: processing update name={:?} attribute={:?} text={}",
            name,
            attr_str,
            shorten_middle(item.text.as_str(), 40)
        );

        let attr = match CharacterAttribute::try_from(attr_str) {
            Ok(a) => a,
            Err(_) => {
                warn!(
                    "character_updater: unknown attribute {:?} for {:?}, skip",
                    attr_str, name
                );
                recorder.record_character_section(
                    update_id,
                    name,
                    attr_str,
                    None,
                    item.text.as_str(),
                    false,
                    Some("unknown attribute"),
                );
                continue;
            }
        };

        // 抽出/マージ LLM が誤って先頭に付与した属性ラベル(「口調：」等)を取り除く。
        let new_text_owned = strip_attribute_label(item.text.as_str(), &attr);
        let new_text = new_text_owned.as_str();

        // 対応ファイルを探す(このターンで新規作成したファイルも char_files に含まれる)
        let target_file = find_character_file(&char_files, name).cloned();

        if let Some(file_path) = target_file {
            // ---- 既存ファイルへの操作 ----
            let file_content = match tokio::fs::read_to_string(&file_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "character_updater: failed to read character file {:?}: {}",
                        file_path, e
                    );
                    recorder.record_character_section(
                        update_id,
                        name,
                        attr_str,
                        None,
                        new_text,
                        false,
                        Some("failed to read character file"),
                    );
                    continue;
                }
            };

            let chars = parse_all_content(&file_content);
            // 見出しの部分一致に加え、登録済み aliases との完全一致でも同一人物と判定する
            // (LLM が別呼称で返してきた場合の重複登録を防ぐ)。
            let char_entry = chars
                .iter()
                .find(|(k, entry)| k.contains(name) || entry.aliases.iter().any(|a| a == name));

            let (old_text_opt, new_content_opt): (Option<String>, Option<String>) = match char_entry
            {
                Some((heading, entry)) => {
                    // キャラ見出しが存在する。alias 経由で一致した場合に備え、
                    // セクション操作には見出しキー全文を使う(見出しは自分自身を contains する)。
                    let section_name = heading.as_str();
                    if let Some(sec) = entry.sections.iter().find(|s| s.tags.contains(&attr)) {
                        // 既存属性セクション → マージ
                        // Alias(呼称)は名前の列挙なので LLM 意味マージを使わず、
                        // 決定的な箇条書きマージ(merge_alias_bodies)で崩れを防ぐ。
                        let old = sec.text.trim().to_string();
                        let merge_result: Result<Option<String>, crate::llm::LlmError> =
                            if attr == CharacterAttribute::Alias {
                                Ok(merge_alias_bodies(&old, new_text, name))
                            } else {
                                merge_section_text_semantically(llm_client, name, &attr, &old, new_text)
                                    .await
                            };
                        let merged = match merge_result {
                            Ok(Some(merged)) => merged,
                            Ok(None) => {
                                debug!(
                                    "character_updater: {}/{} no change to merge, skip",
                                    name, attr_str
                                );
                                recorder.record_character_section(
                                    update_id,
                                    name,
                                    attr_str,
                                    Some(&old),
                                    new_text,
                                    false,
                                    Some("no change"),
                                );
                                continue;
                            }
                            Err(e) => {
                                error!("character_updater: semantic merge failed: {}", e);
                                recorder.record_character_section(
                                    update_id,
                                    name,
                                    attr_str,
                                    Some(&old),
                                    new_text,
                                    false,
                                    Some("semantic merge failed"),
                                );
                                continue;
                            }
                        };
                        // 意味マージ LLM がラベルを付けてきた場合の保険(Alias は既に箇条書きなので不要)。
                        let merged = if attr == CharacterAttribute::Alias {
                            merged
                        } else {
                            strip_attribute_label(&merged, &attr)
                        };
                        match replace_section(&file_content, section_name, &attr, &merged) {
                            Some(c) => (Some(old), Some(c)),
                            None => {
                                warn!(
                                    "character_updater: replace_section failed for {}/{}",
                                    name, attr_str
                                );
                                recorder.record_character_section(
                                    update_id,
                                    name,
                                    attr_str,
                                    Some(&old),
                                    new_text,
                                    false,
                                    Some("replace_section failed"),
                                );
                                continue;
                            }
                        }
                    } else {
                        // 属性セクションが未存在 → 追記(Alias は箇条書きへ正規化)
                        let Some(body) = build_new_section_body(&attr, new_text, name) else {
                            debug!(
                                "character_updater: {}/{} no aliases to add, skip",
                                name, attr_str
                            );
                            recorder.record_character_section(
                                update_id,
                                name,
                                attr_str,
                                None,
                                new_text,
                                false,
                                Some("no change"),
                            );
                            continue;
                        };
                        match append_attribute_section(
                            &file_content,
                            section_name,
                            attr.canonical_heading(),
                            &body,
                        ) {
                            Some(c) => (None, Some(c)),
                            None => {
                                warn!(
                                    "character_updater: append_attribute_section failed for {}/{}",
                                    name, attr_str
                                );
                                recorder.record_character_section(
                                    update_id,
                                    name,
                                    attr_str,
                                    None,
                                    new_text,
                                    false,
                                    Some("append_attribute_section failed"),
                                );
                                continue;
                            }
                        }
                    }
                }
                None => {
                    // 単一ファイル形式の新規キャラ → キャラブロックを末尾追記(Alias は箇条書きへ正規化)
                    let Some(body) = build_new_section_body(&attr, new_text, name) else {
                        debug!(
                            "character_updater: {}/{} no aliases to add, skip",
                            name, attr_str
                        );
                        recorder.record_character_section(
                            update_id,
                            name,
                            attr_str,
                            None,
                            new_text,
                            false,
                            Some("no change"),
                        );
                        continue;
                    };
                    let c = append_new_character(&file_content, name, attr.canonical_heading(), &body);
                    (None, Some(c))
                }
            };

            if let Some(content) = new_content_opt {
                match tokio::fs::write(&file_path, format_markdown(&content)).await {
                    Ok(_) => {
                        info!("character_updater: updated {}/{}", name, attr_str);
                        recorder.record_character_section(
                            update_id,
                            name,
                            attr_str,
                            old_text_opt.as_deref(),
                            new_text,
                            true,
                            None,
                        );
                    }
                    Err(e) => {
                        error!("character_updater: failed to write {:?}: {}", file_path, e);
                        recorder.record_character_section(
                            update_id,
                            name,
                            attr_str,
                            old_text_opt.as_deref(),
                            new_text,
                            false,
                            Some("write failed"),
                        );
                    }
                }
            }
        } else {
            // ---- ファイルが見つからない → フォルダ形式の新規キャラ(Alias は箇条書きへ正規化) ----
            let Some(body) = build_new_section_body(&attr, new_text, name) else {
                debug!(
                    "character_updater: {}/{} no aliases to add, skip",
                    name, attr_str
                );
                recorder.record_character_section(
                    update_id,
                    name,
                    attr_str,
                    None,
                    new_text,
                    false,
                    Some("no change"),
                );
                continue;
            };
            let chars_dir = workspace.join("characters");
            match create_character_file(&chars_dir, name, attr.canonical_heading(), &body).await {
                Some(new_path) => {
                    info!("character_updater: created {}", new_path.display());
                    recorder.record_character_section(
                        update_id, name, attr_str, None, new_text, true, None,
                    );
                    char_files.push(new_path);
                }
                None => {
                    warn!(
                        "character_updater: failed to create character file for {:?}",
                        name
                    );
                    recorder.record_character_section(
                        update_id,
                        name,
                        attr_str,
                        None,
                        new_text,
                        false,
                        Some("failed to create character file"),
                    );
                }
            }
        }
    }
}

/// ファイル一覧からキャラ名に対応するファイルを探す。
/// - 単一ファイル形式(`characters.md`): 全キャラを束ねる集約ファイルなので、名前に関係なくヒットさせる。
/// - フォルダ形式(`characters/<名>.md`): ファイル名(stem)の前方一致で個別ファイルを探す。
fn find_character_file<'a>(files: &'a [PathBuf], name: &str) -> Option<&'a PathBuf> {
    files.iter().find(|p| {
        p.file_stem()
            .map(|s| {
                let stem = s.to_string_lossy();
                stem == "characters" || stem.starts_with(name)
            })
            .unwrap_or(false)
    })
}

/// Markdown テキストの見出し構造を解析し、キャラクターレベルと属性レベルを返す。
/// `main.rs` の `detect_char_level_str` (comrak AST ベース) に委譲する。
/// 検出できない場合は `None`。
fn detect_levels(file_text: &str) -> Option<(u8, u8)> {
    let cl = crate::detect_char_level_str(file_text);
    (cl != 0).then(|| (cl, cl + 1))
}

/// 指定キャラクターの見出しブロック内に、新しい属性セクションを追記する。
/// キャラクター見出しが見つからない場合は `None`。
fn append_attribute_section(
    file_text: &str,
    char_name: &str,
    attr_heading: &str,
    body: &str,
) -> Option<String> {
    let lines: Vec<&str> = file_text.lines().collect();
    let mut in_fence = false;
    let mut char_level: Option<u8> = None;
    let mut found_char = false;
    let mut insert_before = lines.len(); // デフォルト: ファイル末尾

    for (i, &line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        let trimmed = line.trim_end();
        if !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.bytes().take_while(|&b| b == b'#').count() as u8;
        let heading_text = trimmed[level as usize..].trim();

        if !found_char {
            if heading_text.contains(char_name) {
                found_char = true;
                char_level = Some(level);
            }
        } else if level <= char_level.unwrap() {
            // このキャラクターブロックの終端
            insert_before = i;
            break;
        }
    }

    if !found_char {
        return None;
    }

    let cl = char_level.unwrap();
    let attr_prefix = "#".repeat((cl + 1) as usize);
    let new_section = format!("\n{} {}\n{}\n", attr_prefix, attr_heading, body.trim_end());

    let mut out = String::new();
    for (i, &line) in lines.iter().enumerate() {
        if i == insert_before {
            out.push_str(&new_section);
        }
        out.push_str(line);
        out.push('\n');
    }
    if insert_before >= lines.len() {
        out.push_str(&new_section);
    }

    Some(out)
}

/// ファイル末尾に新規キャラクターのブロックを追記して返す。
/// 見出しレベルは既存ファイルから推測し、不明な場合は `#`/`##` を使用する。
fn append_new_character(
    file_text: &str,
    char_name: &str,
    attr_heading: &str,
    body: &str,
) -> String {
    let (cl, al) = detect_levels(file_text).unwrap_or((1, 2));
    let char_prefix = "#".repeat(cl as usize);
    let attr_prefix = "#".repeat(al as usize);
    let new_block = format!(
        "\n{} {}\n\n{} {}\n{}\n",
        char_prefix,
        char_name,
        attr_prefix,
        attr_heading,
        body.trim_end()
    );

    let mut out = file_text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&new_block);
    out
}

/// ファイル stem として使えない文字(`/ \ : * ? " < > |` および制御文字)を `_` に置換する。
/// キャラクター見出し(`# name`)には原名を使うため、このサニタイズはパス生成のみに適用する。
fn sanitize_file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// `chars_dir/<name>.md` を新規作成してパスを返す。作成に失敗した場合は `None`。
/// - ファイル名は `sanitize_file_stem` でサニタイズする(見出しは元の `name` を維持)。
/// - `chars_dir` が存在しない場合は `create_dir_all` で生成する。
async fn create_character_file(
    chars_dir: &Path,
    name: &str,
    attr_heading: &str,
    body: &str,
) -> Option<PathBuf> {
    let stem = sanitize_file_stem(name);
    if stem.trim_matches('_').is_empty() {
        error!(
            "character_updater: cannot convert character name {:?} to a file name",
            name
        );
        return None;
    }
    if let Err(e) = tokio::fs::create_dir_all(chars_dir).await {
        error!(
            "character_updater: failed to create characters directory {:?}: {}",
            chars_dir, e
        );
        return None;
    }
    let path = chars_dir.join(format!("{}.md", stem));
    // 見出しには原名を使う
    let content = format!("# {}\n\n## {}\n\n{}\n", name, attr_heading, body.trim_end());
    match tokio::fs::write(&path, format_markdown(&content)).await {
        Ok(_) => {
            debug!("character_updater: created {:?}", path);
            Some(path)
        }
        Err(e) => {
            error!("character_updater: failed to create {:?}: {}", path, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(180);
    const MIN: usize = 1000;

    #[test]
    fn test_idle_trigger_none_while_active() {
        // gap が idle 未満 → まだ打鍵継続中とみなして None
        assert_eq!(
            idle_trigger(2000, Duration::from_secs(10), IDLE, MIN),
            Trigger::None
        );
    }

    #[test]
    fn test_idle_trigger_none_when_empty() {
        // 何も溜まっていなければ gap によらず None
        assert_eq!(
            idle_trigger(0, Duration::from_secs(600), IDLE, MIN),
            Trigger::None
        );
    }

    #[test]
    fn test_idle_trigger_fire() {
        // idle 経過かつ min_chars 以上 → Fire
        assert_eq!(
            idle_trigger(1200, Duration::from_secs(200), IDLE, MIN),
            Trigger::Fire
        );
    }

    #[test]
    fn test_idle_trigger_clear_stale() {
        // idle 経過だが min_chars 未満 → ClearStale
        assert_eq!(
            idle_trigger(500, Duration::from_secs(200), IDLE, MIN),
            Trigger::ClearStale
        );
    }

    const SAMPLE_MD: &str = "\
# Story
## ジェフ（艦長）
### 背景
- 予備役上がり。
### 性格・口調
- 落ち着いている。
## シルビア
### 性格
- 真面目。
";

    #[test]
    fn test_replace_section_found() {
        let result = replace_section(
            SAMPLE_MD,
            "ジェフ",
            &CharacterAttribute::Background,
            "- 予備役上がりで、元警備隊員。\n- フォン・ブラウン出身。",
        );
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(out.contains("予備役上がりで、元警備隊員"));
        assert!(out.contains("元警備隊員"));
        assert!(out.contains("フォン・ブラウン出身"));
        assert!(
            out.contains("落ち着いている"),
            "他セクションが保持されること"
        );
        assert!(out.contains("シルビア"), "別キャラが保持されること");
    }

    #[test]
    fn test_build_semantic_merge_prompt_requests_meaningful_merge() {
        let prompt =
            build_semantic_merge_prompt("ジェフ", "背景", "- 予備役上がり。", "- 元警備隊員。");
        assert!(prompt.contains("意味が自然につながるようにマージ"));
        assert!(prompt.contains("両方を機械的に並べず"));
        assert!(prompt.contains("矛盾しない表現へ合成"));
        assert!(prompt.contains("- 予備役上がり。"));
        assert!(prompt.contains("- 元警備隊員。"));
    }

    #[test]
    fn test_sanitize_merged_section_text_strips_code_fence() {
        let result = sanitize_merged_section_text("```markdown\n- 統合後。\n```");
        assert_eq!(result, "- 統合後。");
    }

    // ---- format_markdown のテスト ----

    #[test]
    fn test_format_markdown_normalizes_headings_and_bullets() {
        // 見出し直後に本文が続く・箇条書き記号が混在する乱れた入力を正規化する
        let messy = "## ジェフ\n### 性格\n* 落ち着いている。\n+ 冷静。\n### 呼称\n- クライン艦長";
        let formatted = format_markdown(messy);
        assert!(
            formatted.contains("## ジェフ\n\n### 性格"),
            "見出しの後に空行が入ること: {formatted:?}"
        );
        assert!(
            formatted.contains("- 落ち着いている。"),
            "箇条書きが '-' に統一されること: {formatted:?}"
        );
        assert!(
            formatted.contains("- 冷静。"),
            "箇条書きが '-' に統一されること: {formatted:?}"
        );
    }

    #[test]
    fn test_format_markdown_idempotent() {
        let once = format_markdown(SAMPLE_MD);
        let twice = format_markdown(&once);
        assert_eq!(once, twice, "整形は冪等であること");
    }

    #[test]
    fn test_format_markdown_keeps_parse_all_content_compatible() {
        // 整形後もキャラ・セクション・aliases が従来どおりパースできること
        let md = "\
# Story
## ジェフ（艦長）
### 呼称
- クライン艦長
### 性格
- 落ち着いている。
## シルビア
### 性格
- 真面目。
";
        let chars = parse_all_content(&format_markdown(md));
        let jeff = chars
            .get("ジェフ（艦長）")
            .expect("整形後もジェフがパースできること");
        assert!(jeff.aliases.contains(&"クライン艦長".to_string()));
        assert!(
            jeff.sections
                .iter()
                .any(|s| s.tags.contains(&CharacterAttribute::Personality)),
            "性格セクションが保持されること"
        );
        assert!(chars.contains_key("シルビア"), "別キャラも保持されること");
    }

    #[test]
    fn test_format_markdown_handles_edge_inputs() {
        // 空文字列・見出しなしプレーンテキストでも panic しない
        assert_eq!(format_markdown(""), "");
        let plain = format_markdown("本文だけ、見出しなし。");
        assert!(plain.contains("本文だけ、見出しなし。"));
    }

    // ---- strip_merge_preamble / extract_merged_text のテスト ----

    #[test]
    fn test_strip_merge_preamble_removes_known_label() {
        let out = strip_merge_preamble("マージ後の文章：\n親しい友人には率直に話す。");
        assert_eq!(out, "親しい友人には率直に話す。");
    }

    #[test]
    fn test_strip_merge_preamble_halfwidth_colon() {
        let out = strip_merge_preamble("結果:\n落ち着いた性格。");
        assert_eq!(out, "落ち着いた性格。");
    }

    #[test]
    fn test_strip_merge_preamble_leaves_unknown_first_line() {
        // 未知のラベルや実際の本文(1行目に本文が来るケース)は変更しない
        let out = strip_merge_preamble("落ち着いた性格。\n友人には率直。");
        assert_eq!(out, "落ち着いた性格。\n友人には率直。");
    }

    #[test]
    fn test_strip_merge_preamble_does_not_touch_content_with_colon() {
        // 「地の文から推定：〜」のように内容付きの1行目は対象外(コロン後が空でないため非マッチ)
        let out = strip_merge_preamble("地の文から推定：常に冷静。\n次の行。");
        assert_eq!(out, "地の文から推定：常に冷静。\n次の行。");
    }

    #[test]
    fn test_strip_merge_preamble_single_line_untouched() {
        // 改行が無い(＝前置きと本文が分離できない)場合は変更しない
        let out = strip_merge_preamble("マージ後の文章：落ち着いた性格。");
        assert_eq!(out, "マージ後の文章：落ち着いた性格。");
    }

    #[test]
    fn test_extract_merged_text_from_structured_json() {
        let raw = r#"{"merged_text": "落ち着いた性格。"}"#;
        assert_eq!(extract_merged_text(raw), "落ち着いた性格。");
    }

    #[test]
    fn test_extract_merged_text_json_with_preamble_inside() {
        let raw = r#"{"merged_text": "マージ後の文章：\n落ち着いた性格。"}"#;
        assert_eq!(extract_merged_text(raw), "落ち着いた性格。");
    }

    #[test]
    fn test_extract_merged_text_plain_text_fallback() {
        let raw = "```markdown\nマージ後の文章：\n落ち着いた性格。\n```";
        assert_eq!(extract_merged_text(raw), "落ち着いた性格。");
    }

    // ---- strip_attribute_label のテスト ----

    #[test]
    fn test_strip_attribute_label_single_line_fullwidth_colon() {
        let out = strip_attribute_label(
            "口調：親しい友人に対してはタメ口で話す。",
            &CharacterAttribute::Expression,
        );
        assert_eq!(out, "親しい友人に対してはタメ口で話す。");
    }

    #[test]
    fn test_strip_attribute_label_halfwidth_colon() {
        let out = strip_attribute_label("呼称:飛騨艦長、高柳大佐", &CharacterAttribute::Alias);
        assert_eq!(out, "飛騨艦長、高柳大佐");
    }

    #[test]
    fn test_strip_attribute_label_english_key() {
        let out = strip_attribute_label("aliases：飛騨艦長", &CharacterAttribute::Alias);
        assert_eq!(out, "飛騨艦長");
    }

    #[test]
    fn test_strip_attribute_label_multiline_empty_after_colon() {
        let out = strip_attribute_label(
            "呼称：\n- 飛騨艦長\n- 高柳大佐",
            &CharacterAttribute::Alias,
        );
        assert_eq!(out, "- 飛騨艦長\n- 高柳大佐");
    }

    #[test]
    fn test_strip_attribute_label_does_not_touch_sub_label() {
        // 「一人称」は Expression の同義語だが canonical_heading(「口調」)でも
        // 英語 enum 値(「expression」)でもないため、情報を持つサブラベルとして残す
        let out = strip_attribute_label("一人称：私", &CharacterAttribute::Expression);
        assert_eq!(out, "一人称：私");
    }

    #[test]
    fn test_strip_attribute_label_no_label_untouched() {
        let out = strip_attribute_label("冷静沈着。", &CharacterAttribute::Personality);
        assert_eq!(out, "冷静沈着。");
    }

    #[test]
    fn test_strip_attribute_label_wrong_attribute_label_untouched() {
        // 「背景：」は Personality のラベルとして一致しないので除去しない
        let out = strip_attribute_label("背景：過去に事故に遭った。", &CharacterAttribute::Personality);
        assert_eq!(out, "背景：過去に事故に遭った。");
    }

    // ---- merge_alias_bodies のテスト ----

    #[test]
    fn test_merge_alias_bodies_appends_new_only() {
        // old_body は parse_all_content 経由(comrak が箇条書きをプレーンテキスト化した後)の
        // 実際の sec.text を想定しているため、Markdown の "- " マーカーは付かない。
        let merged = merge_alias_bodies("クライン艦長", "飛騨艦長、高柳大佐", "ジェフ").unwrap();
        assert_eq!(merged, "- クライン艦長\n- 飛騨艦長\n- 高柳大佐");
    }

    #[test]
    fn test_merge_alias_bodies_no_new_returns_none() {
        let merged = merge_alias_bodies("クライン艦長・ジェフリー", "クライン艦長", "ジェフ");
        assert_eq!(merged, None);
    }

    #[test]
    fn test_merge_alias_bodies_excludes_char_name_itself() {
        let merged = merge_alias_bodies("", "ジェフ、クライン艦長", "ジェフ").unwrap();
        assert_eq!(merged, "- クライン艦長");
    }

    #[test]
    fn test_merge_alias_bodies_empty_old_and_new_returns_none() {
        assert_eq!(merge_alias_bodies("", "", "ジェフ"), None);
    }

    #[test]
    fn test_merge_alias_bodies_self_heals_legacy_labeled_entry() {
        // 過去のバグで「呼称：飛騨艦長」が単一エントリのまま保存されているケース。
        // 新規抽出で「飛騨艦長」(重複)と「高柳大佐」(新規)が来たとき、
        // ラベル混入トークンを正規化してから比較するため、重複登録されず「呼称：」ラベルも消える。
        let old_body = "呼称：飛騨艦長";
        let merged = merge_alias_bodies(old_body, "飛騨艦長、高柳大佐", "ジェフ").unwrap();
        assert_eq!(merged, "- 飛騨艦長\n- 高柳大佐");
    }

    #[test]
    fn test_replace_section_can_still_replace_body() {
        let result = replace_section(
            SAMPLE_MD,
            "ジェフ",
            &CharacterAttribute::Background,
            "- 元警備隊員で予備役上がり。",
        );
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(out.contains("元警備隊員で予備役上がり"));
        assert!(
            !out.contains("- 予備役上がり。\n"),
            "replace_section 自体は指定本文へ差し替えること"
        );
        assert!(
            out.contains("落ち着いている"),
            "他セクションが保持されること"
        );
        assert!(out.contains("シルビア"), "別キャラが保持されること");
    }

    #[test]
    fn test_replace_section_not_found() {
        let result = replace_section(
            SAMPLE_MD,
            "ジェフ",
            &CharacterAttribute::Appearance,
            "特になし",
        );
        // appearance セクションはないので None
        assert!(result.is_none());
    }

    #[test]
    fn test_replace_section_other_char_untouched() {
        let result = replace_section(
            SAMPLE_MD,
            "シルビア",
            &CharacterAttribute::Personality,
            "- 真面目だが、緊張しやすい面もある。",
        );
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(
            out.contains("落ち着いている"),
            "ジェフのセクションが保持されること"
        );
        assert!(
            out.contains("- 真面目だが、緊張しやすい面もある。"),
            "意味的に統合されたテキストを書けること"
        );
    }

    #[test]
    fn test_find_character_file_single() {
        // 単一ファイル形式: characters.md はどのキャラ名でもヒットする(集約ファイル)
        let files = vec![PathBuf::from("/ws/characters.md")];
        assert_eq!(
            find_character_file(&files, "ジェフ"),
            Some(&files[0]),
            "単一 characters.md は名前に関係なくヒットすること"
        );
        assert_eq!(find_character_file(&files, "シルビア"), Some(&files[0]));
    }

    #[test]
    fn test_find_character_file_folder() {
        // フォルダ形式: characters/<名>.md はファイル名の前方一致で個別に探す
        let files = vec![
            PathBuf::from("/ws/characters/ジェフ.md"),
            PathBuf::from("/ws/characters/シルビア.md"),
        ];
        assert_eq!(find_character_file(&files, "ジェフ"), Some(&files[0]));
        assert_eq!(find_character_file(&files, "シルビア"), Some(&files[1]));
        assert_eq!(find_character_file(&files, "存在しない"), None);
    }

    #[test]
    fn test_detect_levels_with_story_heading() {
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
        assert_eq!(detect_levels(md), Some((2, 3)));
    }

    #[test]
    fn test_detect_levels_top_level_chars() {
        // # キャラ / ## 属性 の構造
        let md = "\
# ジェフ
## 性格
- 落ち着いている。
# シルビア
## 背景
- 不明。
";
        assert_eq!(detect_levels(md), Some((1, 2)));
    }

    #[test]
    fn test_detect_levels_empty() {
        assert_eq!(detect_levels(""), None);
        assert_eq!(detect_levels("本文だけ、見出しなし。"), None);
    }

    #[test]
    fn test_append_attribute_section_to_existing_char() {
        let md = "\
# Story
## ジェフ（艦長）
### 性格
- 落ち着いている。
## シルビア
### 背景
- 不明。
";
        let result = append_attribute_section(md, "ジェフ", "外見", "- 長身で白髪。");
        assert!(result.is_some());
        let out = result.unwrap();
        // 新属性がジェフのブロック内（シルビアの前）に挿入される
        let jeff_end = out.find("## シルビア").unwrap_or(out.len());
        assert!(out.contains("### 外見"), "属性見出しがあること");
        assert!(
            out.find("### 外見").unwrap() < jeff_end,
            "シルビアより前に挿入されること"
        );
        assert!(out.contains("落ち着いている"), "既存属性が保持されること");
        assert!(out.contains("シルビア"), "他キャラが保持されること");
    }

    #[test]
    fn test_append_attribute_section_at_eof() {
        // キャラが最後のブロック(次の同レベル見出しがない)
        let md = "\
## ジェフ
### 性格
- 落ち着いている。
";
        let result = append_attribute_section(md, "ジェフ", "外見", "- 長身。");
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(out.contains("### 外見"), "属性見出しがあること");
        assert!(out.contains("長身"), "属性本文があること");
        assert!(out.contains("落ち着いている"), "既存属性が保持されること");
    }

    #[test]
    fn test_append_attribute_section_char_not_found() {
        let md = "## ジェフ\n### 性格\n- 落ち着いている。\n";
        let result = append_attribute_section(md, "存在しない", "外見", "- 不明。");
        assert!(result.is_none());
    }

    #[test]
    fn test_append_new_character() {
        let md = "\
# Story
## ジェフ
### 性格
- 落ち着いている。
";
        let out = append_new_character(md, "新キャラ", "背景", "- 謎の人物。");
        assert!(out.contains("## 新キャラ"), "新キャラ見出しがあること");
        assert!(out.contains("### 背景"), "属性見出しがあること");
        assert!(out.contains("謎の人物"), "属性本文があること");
        assert!(out.contains("落ち着いている"), "既存キャラが保持されること");
        let jeff_pos = out.find("## ジェフ").unwrap();
        let new_pos = out.find("## 新キャラ").unwrap();
        assert!(new_pos > jeff_pos, "新キャラが末尾に追加されること");
    }

    #[test]
    fn test_append_new_character_empty_file() {
        // 空ファイルはデフォルトレベル(1/2)を使用
        let out = append_new_character("", "新キャラ", "性格", "- 明るい。");
        assert!(out.contains("# 新キャラ"), "レベル1のキャラ見出し");
        assert!(out.contains("## 性格"), "レベル2の属性見出し");
        assert!(out.contains("明るい"), "属性本文");
    }

    #[test]
    fn test_sanitize_file_stem_normal() {
        // 通常の日本語名はそのまま
        assert_eq!(sanitize_file_stem("ジェフ"), "ジェフ");
        assert_eq!(sanitize_file_stem("シルビア・アロン"), "シルビア・アロン");
    }

    #[test]
    fn test_sanitize_file_stem_invalid_chars() {
        // Windows/POSIX で使えない文字は '_' に置換される
        assert_eq!(sanitize_file_stem("alice/bob"), "alice_bob");
        assert_eq!(sanitize_file_stem("a:b"), "a_b");
        assert_eq!(sanitize_file_stem("a*b?c"), "a_b_c");
        assert_eq!(sanitize_file_stem("a<b>c|d"), "a_b_c_d");
        assert_eq!(sanitize_file_stem("a\"b"), "a_b");
        assert_eq!(sanitize_file_stem("a\\b"), "a_b");
    }

    #[test]
    fn test_sanitize_file_stem_control_chars() {
        // 制御文字も '_' に置換される
        let name_with_null = "a\x00b";
        assert_eq!(sanitize_file_stem(name_with_null), "a_b");
    }

    // ---- 応答正規化のテスト ----

    #[test]
    fn test_extract_json_strips_fence_and_prose() {
        let resp = "結果は以下です。\n```json\n{\"updates\": []}\n```\n以上。";
        assert_eq!(extract_json(resp), Some("{\"updates\": []}"));
        assert_eq!(extract_json("JSONなし"), None);
    }

    #[test]
    fn test_parse_updates_canonical_form() {
        let resp = r#"{"updates": [
            {"name": "ジェフ", "attribute": "personality", "text": "冷静沈着。"},
            {"name": "ジェフ", "attribute": "aliases", "text": "クライン艦長"}
        ]}"#;
        let items = parse_updates(resp).expect("正規形式はパースできること");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "ジェフ");
        assert_eq!(items[0].attribute, "personality");
        assert_eq!(items[0].text, "冷静沈着。");
        assert_eq!(items[1].attribute, "aliases");
    }

    #[test]
    fn test_parse_updates_empty_canonical() {
        // 抽出ゼロの正規応答は空リストとして受理(リトライしない)
        let items = parse_updates(r#"{"updates": []}"#);
        assert_eq!(items, Some(Vec::new()));
    }

    #[test]
    fn test_parse_updates_text_object_coerced() {
        // text にオブジェクトを返すスキーマ違反 → 可読文字列へフラット化して救済
        let resp = r#"{"updates": [
            {"name": "高柳", "attribute": "appearance", "text": {"description": "特になし"}}
        ]}"#;
        let items = parse_updates(resp).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "description: 特になし");
    }

    #[test]
    fn test_parse_updates_characters_form_salvaged() {
        // 実際に観測された「キャラクター単位のネスト形式」応答の救済変換
        let resp = r#"{"characters": [
            {
                "name": "高柳",
                "aliases": ["飛騨艦長"],
                "role": "軍人",
                "appearance": {"description": "特になし"},
                "personality": "多忙だが友人には率直。",
                "expression": {
                    "tone": "タメ口",
                    "first_person": "私",
                    "representative_quotes": ["わかったことを聞くな、近藤。", "……そっちか"]
                },
                "relationship": {"近藤": "中学からの腐れ縁。", "原さん": "活躍を知っている。"},
                "unknown_key": "捨てられること"
            }
        ]}"#;
        let items = parse_updates(resp).expect("characters形式は救済変換されること");
        let attrs: Vec<&str> = items.iter().map(|i| i.attribute.as_str()).collect();
        assert!(attrs.contains(&"aliases"));
        assert!(attrs.contains(&"role"));
        assert!(attrs.contains(&"personality"));
        assert!(attrs.contains(&"expression"));
        assert!(attrs.contains(&"relationship"));
        assert!(
            !attrs.contains(&"unknown_key"),
            "属性として解釈できないキーは捨てられること"
        );
        assert!(items.iter().all(|i| i.name == "高柳"));

        let aliases = items.iter().find(|i| i.attribute == "aliases").unwrap();
        assert_eq!(aliases.text, "飛騨艦長");

        let expr = items.iter().find(|i| i.attribute == "expression").unwrap();
        assert!(expr.text.contains("tone: タメ口"));
        assert!(expr.text.contains("わかったことを聞くな、近藤。"));

        let rel = items
            .iter()
            .find(|i| i.attribute == "relationship")
            .unwrap();
        assert!(rel.text.contains("近藤: 中学からの腐れ縁。"));
    }

    #[test]
    fn test_parse_updates_unparseable_returns_none() {
        // どの形式でも解釈できない → None(リトライ対象)
        assert_eq!(parse_updates("申し訳ありませんが、できません。"), None);
        assert_eq!(parse_updates(r#"{"result": "ok"}"#), None);
        // characters 形式でも1件も取れなければ None
        assert_eq!(parse_updates(r#"{"characters": [{"番号": 1}]}"#), None);
    }

    #[test]
    fn test_value_to_text_flattening() {
        use serde_json::json;
        assert_eq!(value_to_text(&json!("文字列")), "文字列");
        assert_eq!(value_to_text(&json!(["a", "b"])), "a\nb");
        assert_eq!(value_to_text(&json!({"k": "v"})), "k: v");
        // ネスト: 複数行になる値はキーの後に改行
        assert_eq!(
            value_to_text(&json!({"quotes": ["a", "b"]})),
            "quotes:\na\nb"
        );
    }

}
