use crate::llm::{Content, LlmInterface, ModelCapability};
use crate::{CharacterAttribute, FlightRecorder, parse_all_content, shorten_middle, split_aliases};
use dashmap::DashMap;
use genai::chat::{ChatResponseFormat, JsonSpec};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use serde_json::Value;
use std::collections::HashMap;
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

/// バッチマージの1物理セクションぶんのタスク。
/// 同一物理セクションへ解決される複数の更新項目は `new_body` に "\n" 連結で合流させ、
/// 「1物理宛先 = 1タスク」を厳守する(後勝ち上書きによるデータ欠損の防止)。
#[derive(Debug, PartialEq)]
struct SectionMergeTask {
    /// 物理セクションの表示見出し(プロンプト表示＆結果突き合わせの echo キー)。
    heading: String,
    /// 現在のセクション本文(計画時点のスナップショット、trim 済み)。
    old_body: String,
    /// このセクションに合流する新情報(複数 item は "\n" 連結)。
    new_body: String,
}

/// キャラ単位のマージ対象グループ。バッチプロンプトの1キャラブロックに対応する。
#[derive(Debug)]
struct CharMergeGroup {
    file: PathBuf,
    /// プロンプト表示＆echo キー。通常はキャラ見出しキー全文と同一。
    /// 別ファイル間で見出しが衝突した場合のみ「見出し（ファイル名）」で一意化する。
    display_name: String,
    sections: Vec<SectionMergeTask>,
}

/// バッチマージ結果の JSON スキーマ。
/// `{"characters": [{"name", "sections": [{"heading", "merged_text"}]}]}` の入れ子で、
/// name/heading はプロンプトで与えた文字列の復唱を要求する(結果突き合わせキー)。
fn batch_merge_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "characters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "sections": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "heading": { "type": "string" },
                                    "merged_text": { "type": "string" }
                                },
                                "required": ["heading", "merged_text"]
                            }
                        }
                    },
                    "required": ["name", "sections"]
                }
            }
        },
        "required": ["characters"]
    })
}

/// 1キャラぶんのマージ対象をプロンプトのキャラブロックへレンダリングする。
/// 見出し階層(`##`/`###`/`####`)はテンプレートの「# キャラクター一覧」の下に入れ子になる。
fn render_char_group(group: &CharMergeGroup) -> String {
    let mut out = format!("## キャラクター: {}\n", group.display_name);
    for sec in &group.sections {
        out.push_str(&format!(
            "\n### セクション: {}\n\n#### 現在の設定\n{}\n\n#### 新しく本文から読み取れた情報\n{}\n",
            sec.heading,
            sec.old_body.trim(),
            sec.new_body.trim()
        ));
    }
    out
}

/// バッチマージ用プロンプトを `data/prompt_semantic_merge_batch.md` から読み込み、
/// `{{CHARACTERS}}` に全キャラブロックを埋めて返す。テンプレートを読めなければ `None`。
fn build_batch_merge_prompt(groups: &[CharMergeGroup]) -> Option<String> {
    let (template, _) = crate::load_prompt("prompt_semantic_merge_batch.md")?;
    let characters = groups
        .iter()
        .map(render_char_group)
        .collect::<Vec<_>>()
        .join("\n");
    Some(template.replace("{{CHARACTERS}}", &characters))
}

/// バッチマージ応答をパースし、`(name, heading) -> merged_text` のマップを返す。
/// 各 `merged_text` にはコードフェンス除去・前置きラベル除去を適用する。
/// パース不能・スキーマ不一致の要素は黙って読み飛ばす(欠落分は呼び出し側で old 維持)。
fn extract_batch_merges(raw: &str) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let Some(json) = extract_json(raw) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return out;
    };
    let Some(arr) = v.get("characters").and_then(|x| x.as_array()) else {
        return out;
    };
    for ch in arr {
        let Some(name) = ch.get("name").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(secs) = ch.get("sections").and_then(|x| x.as_array()) else {
            continue;
        };
        for sec in secs {
            let Some(heading) = sec.get("heading").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(text) = sec.get("merged_text").and_then(|x| x.as_str()) else {
                continue;
            };
            let cleaned = strip_merge_preamble(&sanitize_merged_section_text(text));
            out.insert(
                (name.trim().to_string(), heading.trim().to_string()),
                cleaned,
            );
        }
    }
    out
}

/// バッチ結果マップから (display_name, heading) の結果を引く。
/// 完全一致で見つからない場合、同名キャラ内で返却見出しを属性正規化して突き合わせる
/// (LLM が「性格・口調」を「性格」へ縮めて復唱したケースの救済)。
/// 候補が複数(曖昧)・皆無なら `None`(呼び出し側で old 維持=書かない)。
fn lookup_merged<'a>(
    merged: &'a HashMap<(String, String), String>,
    display_name: &str,
    heading: &str,
    attr: &CharacterAttribute,
) -> Option<&'a str> {
    if let Some(m) = merged.get(&(display_name.to_string(), heading.to_string())) {
        return Some(m.as_str());
    }
    let mut candidates = merged.iter().filter(|((n, h), _)| {
        n == display_name
            && h.split(['・', '、', ',', '/', ' '])
                .filter_map(|s| CharacterAttribute::try_from(s).ok())
                .any(|a| a == *attr)
    });
    let first = candidates.next();
    match (first, candidates.next()) {
        (Some((_, m)), None) => Some(m.as_str()),
        _ => None,
    }
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

/// 宛先の物理セクションを一意に識別するキー(解決後セクション同一性)。
/// 属性シノニム(「性格」/"personality")や多タグ見出し(「性格・口調」)は
/// 同じ `sec_idx` に解決されるため、このキーで自然に1宛先へ合流する。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SectionKey {
    file: PathBuf,
    char_heading: String,
    sec_idx: usize,
}

/// FlightRecorder へ item 単位で記録するために保持する、正規化済みの元更新項目。
#[derive(Debug, Clone)]
struct RecordInfo {
    name: String,
    attr_str: String,
    new_text: String,
}

/// 計画フェーズが確定する、書き込み高々1回ぶんの解決済み操作。
/// 同一物理宛先への複数の更新項目は必ず1つの操作へ合流させる
/// (適用時の後勝ち上書きによるデータ欠損を構造的に防ぐ。Skip は書き込まないので対象外)。
#[derive(Debug)]
enum ResolvedOp {
    /// 既存の非 Alias セクションへの意味マージ差し替え。マージ本文はバッチ結果マップを `key` で引く。
    Merge {
        file: PathBuf,
        section_name: String,
        attr: CharacterAttribute,
        /// (display_name, セクション見出し) の echo キー。
        key: (String, String),
        old_body: String,
        records: Vec<RecordInfo>,
    },
    /// 既存 Alias セクションの決定的マージ差し替え(本文は計画時点で確定済み)。
    ReplaceAlias {
        file: PathBuf,
        section_name: String,
        old: String,
        merged: String,
        records: Vec<RecordInfo>,
    },
    /// 既存キャラへの新規属性セクション追記。
    AppendSection {
        file: PathBuf,
        section_name: String,
        heading: &'static str,
        body: String,
        records: Vec<RecordInfo>,
    },
    /// 単一ファイル形式への新規キャラブロック追記(全属性を1ブロックへ集約)。
    AppendCharacter {
        file: PathBuf,
        name: String,
        sections: Vec<(&'static str, String)>,
        records: Vec<RecordInfo>,
    },
    /// フォルダ形式の新規キャラファイル作成(全属性を集約)。
    CreateFile {
        name: String,
        sections: Vec<(&'static str, String)>,
        records: Vec<RecordInfo>,
    },
    /// 変更なし・解決不能などの確定スキップ(recorder に記録するだけで書き込まない)。
    Skip {
        record: RecordInfo,
        note: &'static str,
        old: Option<String>,
    },
}

/// 物理セクションの表示見出しをタグ列から再構成する(「性格・口調」等)。
/// `TaggedContent` は元見出し文字列を保持しないため canonical_heading の「・」結合で代用する。
/// echo キーは「こちらが与えた文字列との往復一致」だけが要件なので、原文一致は不要。
fn section_display_heading(tags: &[CharacterAttribute]) -> String {
    tags.iter()
        .map(|t| t.canonical_heading())
        .collect::<Vec<_>>()
        .join("・")
}

/// 未解決(新規)キャラへの更新を、同一キャラにつき1つの CreateFile/AppendCharacter へ集約する。
/// 既出の新規キャラ名が今回の name を含む場合(「ジェフ・クライン」に対する「ジェフ」)も
/// 同一キャラとみなし、逐次適用時の見出し部分一致と挙動を揃える。
fn merge_into_new(
    plan: &mut Vec<ResolvedOp>,
    new_chars: &mut Vec<(Option<PathBuf>, String, usize)>,
    scope: Option<PathBuf>,
    name: &str,
    attr: &CharacterAttribute,
    new_text: &str,
    rec: RecordInfo,
) {
    let existing = new_chars
        .iter()
        .find(|(s, n, _)| *s == scope && n.contains(name))
        .map(|&(_, _, op_idx)| op_idx);

    let Some(op_idx) = existing else {
        match build_new_section_body(attr, new_text, name) {
            Some(body) => {
                let sections = vec![(attr.canonical_heading(), body)];
                let op = match &scope {
                    Some(f) => ResolvedOp::AppendCharacter {
                        file: f.clone(),
                        name: name.to_string(),
                        sections,
                        records: vec![rec],
                    },
                    None => ResolvedOp::CreateFile {
                        name: name.to_string(),
                        sections,
                        records: vec![rec],
                    },
                };
                plan.push(op);
                new_chars.push((scope, name.to_string(), plan.len() - 1));
            }
            None => plan.push(ResolvedOp::Skip {
                record: rec,
                note: "no change",
                old: None,
            }),
        }
        return;
    };

    // 既出の新規キャラ op へ合流。同属性セクションが既にあれば本文へ連結する
    // (plan[op_idx] を可変借用中は plan.push できないため、Skip はフラグ経由で後から積む)。
    let mut skip_rec: Option<RecordInfo> = None;
    if let ResolvedOp::AppendCharacter { sections, records, .. }
    | ResolvedOp::CreateFile { sections, records, .. } = &mut plan[op_idx]
    {
        match sections
            .iter_mut()
            .find(|(h, _)| *h == attr.canonical_heading())
        {
            Some((_, body)) => {
                let addition = if *attr == CharacterAttribute::Alias {
                    merge_alias_bodies(body, new_text, name)
                } else {
                    Some(format!("{}\n{}", body, new_text))
                };
                match addition {
                    Some(b) => {
                        *body = b;
                        records.push(rec);
                    }
                    None => skip_rec = Some(rec),
                }
            }
            None => match build_new_section_body(attr, new_text, name) {
                Some(b) => {
                    sections.push((attr.canonical_heading(), b));
                    records.push(rec);
                }
                None => skip_rec = Some(rec),
            },
        }
    }
    if let Some(record) = skip_rec {
        plan.push(ResolvedOp::Skip {
            record,
            note: "no change",
            old: None,
        });
    }
}

/// (A) 計画フェーズ: 生の更新項目列を、解決済み操作の列とバッチマージ対象グループへ変換する。
/// ファイル内容は `snapshots`(計画時点のスナップショット)から読み、LLM は呼ばない純関数。
/// キャラ・属性の解決はここで1度だけ確定し、適用フェーズでは再判定しない。
fn plan_updates(
    updates: &[UpdateItem],
    char_files: &[PathBuf],
    snapshots: &HashMap<PathBuf, String>,
) -> (Vec<ResolvedOp>, Vec<CharMergeGroup>) {
    let mut plan: Vec<ResolvedOp> = Vec::new();
    let mut groups: Vec<CharMergeGroup> = Vec::new();

    let mut parsed: HashMap<PathBuf, HashMap<String, crate::CharacterEntry>> = HashMap::new();
    // 合流用インデックス群。値は plan / groups 内の位置。
    let mut merge_by_section: HashMap<SectionKey, (usize, usize, usize)> = HashMap::new(); // (group, sec, op)
    let mut alias_by_section: HashMap<SectionKey, usize> = HashMap::new();
    let mut append_by_attr: HashMap<(PathBuf, String, &'static str), usize> = HashMap::new();
    let mut group_by_char: HashMap<(PathBuf, String), usize> = HashMap::new();
    // 新規キャラは見出しの contains 一致で合流させるため線形走査のリストを使う
    let mut new_chars: Vec<(Option<PathBuf>, String, usize)> = Vec::new();

    for item in updates {
        let name = item.name.as_str();
        let attr_str = item.attribute.as_str();
        debug!(
            "character_updater: planning update name={:?} attribute={:?} text={}",
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
                plan.push(ResolvedOp::Skip {
                    record: RecordInfo {
                        name: item.name.clone(),
                        attr_str: item.attribute.clone(),
                        new_text: item.text.clone(),
                    },
                    note: "unknown attribute",
                    old: None,
                });
                continue;
            }
        };

        // 抽出 LLM が誤って先頭に付与した属性ラベル(「口調：」等)を取り除く。
        let new_text = strip_attribute_label(item.text.as_str(), &attr);
        let rec = RecordInfo {
            name: item.name.clone(),
            attr_str: item.attribute.clone(),
            new_text: new_text.clone(),
        };

        // 対応ファイルを探す(見つからなければフォルダ形式の新規キャラとして集約)
        let Some(file) = find_character_file(char_files, name).cloned() else {
            merge_into_new(&mut plan, &mut new_chars, None, name, &attr, &new_text, rec);
            continue;
        };
        let Some(content) = snapshots.get(&file) else {
            plan.push(ResolvedOp::Skip {
                record: rec,
                note: "failed to read character file",
                old: None,
            });
            continue;
        };
        let chars = parsed
            .entry(file.clone())
            .or_insert_with(|| parse_all_content(content));
        // 見出しの部分一致に加え、登録済み aliases との完全一致でも同一人物と判定する
        // (LLM が別呼称で返してきた場合の重複登録を防ぐ)。
        let found = chars
            .iter()
            .find(|(k, entry)| k.contains(name) || entry.aliases.iter().any(|a| a == name))
            .map(|(k, entry)| (k.clone(), entry.clone()));

        let Some((heading, entry)) = found else {
            // 単一ファイル形式の新規キャラ
            merge_into_new(
                &mut plan,
                &mut new_chars,
                Some(file.clone()),
                name,
                &attr,
                &new_text,
                rec,
            );
            continue;
        };

        let Some(sec_idx) = entry.sections.iter().position(|s| s.tags.contains(&attr)) else {
            // 属性セクション未存在 → 追記(同一キャラ×属性の2件目以降は本文へ合流)
            let akey = (file.clone(), heading.clone(), attr.canonical_heading());
            if let Some(&op_idx) = append_by_attr.get(&akey) {
                let mut skip_rec: Option<RecordInfo> = None;
                if let ResolvedOp::AppendSection { body, records, .. } = &mut plan[op_idx] {
                    let addition = if attr == CharacterAttribute::Alias {
                        merge_alias_bodies(body, &new_text, name)
                    } else {
                        Some(format!("{}\n{}", body, new_text))
                    };
                    match addition {
                        Some(b) => {
                            *body = b;
                            records.push(rec);
                        }
                        None => skip_rec = Some(rec),
                    }
                }
                if let Some(record) = skip_rec {
                    plan.push(ResolvedOp::Skip {
                        record,
                        note: "no change",
                        old: None,
                    });
                }
            } else {
                match build_new_section_body(&attr, &new_text, name) {
                    Some(body) => {
                        plan.push(ResolvedOp::AppendSection {
                            file: file.clone(),
                            section_name: heading.clone(),
                            heading: attr.canonical_heading(),
                            body,
                            records: vec![rec],
                        });
                        append_by_attr.insert(akey, plan.len() - 1);
                    }
                    None => {
                        debug!(
                            "character_updater: {}/{} no aliases to add, skip",
                            name, attr_str
                        );
                        plan.push(ResolvedOp::Skip {
                            record: rec,
                            note: "no change",
                            old: None,
                        });
                    }
                }
            }
            continue;
        };

        // 既存属性セクションが存在 → マージ対象
        let skey = SectionKey {
            file: file.clone(),
            char_heading: heading.clone(),
            sec_idx,
        };
        let old = entry.sections[sec_idx].text.trim().to_string();

        if attr == CharacterAttribute::Alias {
            // Alias(呼称)は名前の列挙なので LLM 意味マージを使わず決定的にマージする。
            // 同一物理セクションに非 Alias のマージが既に立っている稀ケースは
            // 安全側でスキップする(1宛先1書き込みの不変条件を守る)。
            if merge_by_section.contains_key(&skey) {
                plan.push(ResolvedOp::Skip {
                    record: rec,
                    note: "conflicting section op",
                    old: Some(old),
                });
                continue;
            }
            if let Some(&op_idx) = alias_by_section.get(&skey) {
                let current = match &plan[op_idx] {
                    ResolvedOp::ReplaceAlias { merged, .. } => merged.clone(),
                    _ => old.clone(),
                };
                match merge_alias_bodies(&current, &new_text, name) {
                    Some(m) => {
                        if let ResolvedOp::ReplaceAlias { merged, records, .. } =
                            &mut plan[op_idx]
                        {
                            *merged = m;
                            records.push(rec);
                        }
                    }
                    None => plan.push(ResolvedOp::Skip {
                        record: rec,
                        note: "no change",
                        old: Some(current),
                    }),
                }
            } else {
                match merge_alias_bodies(&old, &new_text, name) {
                    Some(m) => {
                        plan.push(ResolvedOp::ReplaceAlias {
                            file: file.clone(),
                            section_name: heading.clone(),
                            old: old.clone(),
                            merged: m,
                            records: vec![rec],
                        });
                        alias_by_section.insert(skey, plan.len() - 1);
                    }
                    None => {
                        debug!(
                            "character_updater: {}/{} no change to merge, skip",
                            name, attr_str
                        );
                        plan.push(ResolvedOp::Skip {
                            record: rec,
                            note: "no change",
                            old: Some(old),
                        });
                    }
                }
            }
            continue;
        }

        // 非 Alias → バッチマージタスクへ合流
        if alias_by_section.contains_key(&skey) {
            plan.push(ResolvedOp::Skip {
                record: rec,
                note: "conflicting section op",
                old: Some(old),
            });
            continue;
        }
        if let Some(&(gi, si, op_idx)) = merge_by_section.get(&skey) {
            groups[gi].sections[si].new_body.push('\n');
            groups[gi].sections[si].new_body.push_str(&new_text);
            if let ResolvedOp::Merge { records, .. } = &mut plan[op_idx] {
                records.push(rec);
            }
        } else {
            let gi = match group_by_char.get(&(file.clone(), heading.clone())) {
                Some(&gi) => gi,
                None => {
                    // 別ファイル間で見出しが衝突した場合のみ「見出し（ファイル名）」で echo キーを一意化
                    let mut display = heading.clone();
                    if groups
                        .iter()
                        .any(|g| g.display_name == display && g.file != file)
                    {
                        let stem = file
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        display = format!("{}（{}）", heading, stem);
                    }
                    groups.push(CharMergeGroup {
                        file: file.clone(),
                        display_name: display,
                        sections: Vec::new(),
                    });
                    group_by_char.insert((file.clone(), heading.clone()), groups.len() - 1);
                    groups.len() - 1
                }
            };
            let sec_heading = section_display_heading(&entry.sections[sec_idx].tags);
            groups[gi].sections.push(SectionMergeTask {
                heading: sec_heading.clone(),
                old_body: old.clone(),
                new_body: new_text.clone(),
            });
            plan.push(ResolvedOp::Merge {
                file: file.clone(),
                section_name: heading.clone(),
                attr,
                key: (groups[gi].display_name.clone(), sec_heading),
                old_body: old,
                records: vec![rec],
            });
            merge_by_section.insert(skey, (gi, groups[gi].sections.len() - 1, plan.len() - 1));
        }
    }

    (plan, groups)
}

/// (B) バッチマージ: 全キャラのマージ対象を1回の LLM 呼び出しで処理し、
/// `(display_name, heading) -> merged_text` の結果マップを返す。
/// 応答から期待キーが1件も引けない場合のみ、抽出フェーズと同様に1回だけ修正を要求する
/// (部分欠落は全体リトライせず、適用フェーズで個別に old 維持へフォールバックする)。
async fn run_batch_merge(
    llm_client: &mut dyn LlmInterface,
    groups: &[CharMergeGroup],
) -> Result<HashMap<(String, String), String>, crate::llm::LlmError> {
    let prompt =
        build_batch_merge_prompt(groups).ok_or_else(|| crate::llm::LlmError::GenericError {
            message: "prompt_semantic_merge_batch.md を読み込めませんでした".to_string(),
        })?;
    let schema_str = batch_merge_output_schema().to_string();
    let structured = llm_client
        .capabilities()
        .contains(ModelCapability::STRUCTURED_OUTPUT);
    // 出力上限はセクション数に比例させる(固定値だと複数タスクで途中切れする)
    let total_sections: usize = groups.iter().map(|g| g.sections.len()).sum();
    let max_tokens = (512 + 512 * total_sections as u32).min(8192);

    // chat() がオプションをリセットするため、初回・リトライの両方で設定する
    let set_llm_options = |llm_client: &mut dyn LlmInterface| {
        llm_client.temperature(0.2);
        llm_client.max_tokens(max_tokens);
        llm_client.reasoning_level(0.0);
        if structured {
            llm_client.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                "batch_merged_sections",
                batch_merge_output_schema(),
            )));
        }
    };

    set_llm_options(llm_client);
    if !structured {
        llm_client.add(Content::Text(format!(
            "回答は次のJSON schemaに厳密にしたがって生成せよ。\n\nJSON Schema:\n{}\n\n最終応答はスキーマに適合するJSONのみを出力し、JSON以外の文字は一切含めないこと。",
            schema_str
        )));
    }
    llm_client.add(Content::Text(prompt));
    let resp = llm_client.chat().await?;
    debug!(
        "character_updater: batch merge response received :{}",
        resp
    );
    let mut merged = extract_batch_merges(&resp);

    // 全滅判定: 期待キーが1件も引けない応答はスキーマ不適合とみなす
    let any_hit = |m: &HashMap<(String, String), String>| {
        groups.iter().any(|g| {
            g.sections
                .iter()
                .any(|s| m.contains_key(&(g.display_name.clone(), s.heading.clone())))
        })
    };
    if !any_hit(&merged) {
        warn!("character_updater: batch merge response does not conform to schema, retrying once");
        set_llm_options(llm_client);
        llm_client.add(Content::Text(format!(
            "以下はスキーマに適合しない不正な応答である。JSON schemaに厳密に適合するJSONへ修正して出力し直せ。JSON以外の文字は一切含めないこと。\n\nJSON Schema:\n{}\n\n不正な応答:\n{}",
            schema_str, resp
        )));
        match llm_client.chat().await {
            Ok(retry_resp) => {
                debug!(
                    "character_updater: batch merge retry response received :{}",
                    retry_resp
                );
                merged = extract_batch_merges(&retry_resp);
            }
            Err(e) => error!("character_updater: batch merge retry failed: {}", e),
        }
    }
    Ok(merged)
}

/// records 内の全 item を同一の結果で FlightRecorder へ記録する。
fn record_all(
    recorder: &FlightRecorder,
    update_id: i64,
    records: &[RecordInfo],
    old: Option<&str>,
    success: bool,
    note: Option<&str>,
) {
    for r in records {
        recorder.record_character_section(
            update_id,
            &r.name,
            &r.attr_str,
            old,
            &r.new_text,
            success,
            note,
        );
    }
}

/// Markdown を整形して書き込み、records 全件の成否を記録する。
async fn write_and_record(
    file: &Path,
    content: &str,
    update_id: i64,
    records: &[RecordInfo],
    old: Option<&str>,
    recorder: &FlightRecorder,
) {
    match tokio::fs::write(file, format_markdown(content)).await {
        Ok(_) => {
            for r in records {
                info!("character_updater: updated {}/{}", r.name, r.attr_str);
            }
            record_all(recorder, update_id, records, old, true, None);
        }
        Err(e) => {
            error!("character_updater: failed to write {:?}: {}", file, e);
            record_all(recorder, update_id, records, old, false, Some("write failed"));
        }
    }
}

/// (C) 適用フェーズ: 解決済み操作を逐次適用する。
/// 各操作は書き込み直前にファイルを読み直す(read-fresh)ため、同一ファイル内の
/// 先行書き込みと合成される(replace_section は対象セクションの行区間のみ差し替えるので
/// 他セクションへの書き込みとは干渉しない)。
/// `merged_map` が `None` の場合はバッチマージ自体が失敗している(全 Merge を old 維持で記録)。
async fn apply_plan(
    plan: Vec<ResolvedOp>,
    merged_map: Option<&HashMap<(String, String), String>>,
    update_id: i64,
    workspace: &Path,
    recorder: &FlightRecorder,
) {
    for op in plan {
        match op {
            ResolvedOp::Skip { record, note, old } => {
                debug!(
                    "character_updater: {}/{} skip: {}",
                    record.name, record.attr_str, note
                );
                recorder.record_character_section(
                    update_id,
                    &record.name,
                    &record.attr_str,
                    old.as_deref(),
                    &record.new_text,
                    false,
                    Some(note),
                );
            }
            ResolvedOp::Merge {
                file,
                section_name,
                attr,
                key,
                old_body,
                records,
            } => {
                let looked = merged_map.and_then(|m| lookup_merged(m, &key.0, &key.1, &attr));
                let Some(m) = looked else {
                    // マージ結果が不確定なら書かない(old 維持)。
                    let note = if merged_map.is_some() {
                        "merge missing"
                    } else {
                        "semantic merge failed"
                    };
                    warn!(
                        "character_updater: no batch merge result for {}/{}, keep old",
                        key.0, key.1
                    );
                    record_all(recorder, update_id, &records, Some(&old_body), false, Some(note));
                    continue;
                };
                // 意味マージ LLM がラベルを付けてきた場合の保険。
                let m = strip_attribute_label(m, &attr);
                if m.trim().is_empty() || m.trim() == old_body.trim() {
                    debug!(
                        "character_updater: {}/{} no change to merge, skip",
                        key.0, key.1
                    );
                    record_all(
                        recorder,
                        update_id,
                        &records,
                        Some(&old_body),
                        false,
                        Some("no change"),
                    );
                    continue;
                }
                let content = match tokio::fs::read_to_string(&file).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            "character_updater: failed to read character file {:?}: {}",
                            file, e
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            Some(&old_body),
                            false,
                            Some("failed to read character file"),
                        );
                        continue;
                    }
                };
                match replace_section(&content, &section_name, &attr, &m) {
                    Some(c) => {
                        write_and_record(&file, &c, update_id, &records, Some(&old_body), recorder)
                            .await
                    }
                    None => {
                        warn!(
                            "character_updater: replace_section failed for {}/{}",
                            key.0, key.1
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            Some(&old_body),
                            false,
                            Some("replace_section failed"),
                        );
                    }
                }
            }
            ResolvedOp::ReplaceAlias {
                file,
                section_name,
                old,
                merged,
                records,
            } => {
                let content = match tokio::fs::read_to_string(&file).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            "character_updater: failed to read character file {:?}: {}",
                            file, e
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            Some(&old),
                            false,
                            Some("failed to read character file"),
                        );
                        continue;
                    }
                };
                match replace_section(&content, &section_name, &CharacterAttribute::Alias, &merged)
                {
                    Some(c) => {
                        write_and_record(&file, &c, update_id, &records, Some(&old), recorder)
                            .await
                    }
                    None => {
                        warn!(
                            "character_updater: replace_section failed for {}/呼称",
                            section_name
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            Some(&old),
                            false,
                            Some("replace_section failed"),
                        );
                    }
                }
            }
            ResolvedOp::AppendSection {
                file,
                section_name,
                heading,
                body,
                records,
            } => {
                let content = match tokio::fs::read_to_string(&file).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            "character_updater: failed to read character file {:?}: {}",
                            file, e
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            None,
                            false,
                            Some("failed to read character file"),
                        );
                        continue;
                    }
                };
                match append_attribute_section(&content, &section_name, heading, &body) {
                    Some(c) => {
                        write_and_record(&file, &c, update_id, &records, None, recorder).await
                    }
                    None => {
                        warn!(
                            "character_updater: append_attribute_section failed for {}/{}",
                            section_name, heading
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            None,
                            false,
                            Some("append_attribute_section failed"),
                        );
                    }
                }
            }
            ResolvedOp::AppendCharacter {
                file,
                name,
                sections,
                records,
            } => {
                let content = match tokio::fs::read_to_string(&file).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            "character_updater: failed to read character file {:?}: {}",
                            file, e
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            None,
                            false,
                            Some("failed to read character file"),
                        );
                        continue;
                    }
                };
                let c = append_new_character_block(&content, &name, &sections);
                write_and_record(&file, &c, update_id, &records, None, recorder).await;
            }
            ResolvedOp::CreateFile {
                name,
                sections,
                records,
            } => {
                let chars_dir = workspace.join("characters");
                match create_character_file(&chars_dir, &name, &sections).await {
                    Some(new_path) => {
                        info!("character_updater: created {}", new_path.display());
                        record_all(recorder, update_id, &records, None, true, None);
                    }
                    None => {
                        warn!(
                            "character_updater: failed to create character file for {:?}",
                            name
                        );
                        record_all(
                            recorder,
                            update_id,
                            &records,
                            None,
                            false,
                            Some("failed to create character file"),
                        );
                    }
                }
            }
        }
    }
}

/// 正規化済みの更新項目列を characters/*.md へ適用する。
/// (A) 計画: 全項目を解決済み操作へ変換(同一物理宛先は合流) →
/// (B) バッチマージ: 既存セクションへの意味マージを1回の LLM 呼び出しで実行 →
/// (C) 適用: 操作を逐次適用し、FlightRecorder へ item 単位で記録する。
async fn apply_updates(
    updates: &[UpdateItem],
    update_id: i64,
    char_files: Vec<PathBuf>,
    workspace: &Path,
    recorder: &FlightRecorder,
    llm_client: &mut dyn LlmInterface,
) {
    debug!("character_updater: {} update(s) received", updates.len());

    // (A) 計画: 各ファイルを1回だけスナップショットして宛先を確定する
    let mut snapshots: HashMap<PathBuf, String> = HashMap::new();
    for f in &char_files {
        match tokio::fs::read_to_string(f).await {
            Ok(c) => {
                snapshots.insert(f.clone(), c);
            }
            Err(e) => warn!(
                "character_updater: failed to read character file {:?}: {}",
                f, e
            ),
        }
    }
    let (plan, groups) = plan_updates(updates, &char_files, &snapshots);

    // (B) バッチマージ(対象が無ければ LLM を呼ばない)
    let merged_map = if groups.is_empty() {
        Some(HashMap::new())
    } else {
        match run_batch_merge(llm_client, &groups).await {
            Ok(m) => Some(m),
            Err(e) => {
                error!("character_updater: semantic merge failed: {}", e);
                None
            }
        }
    };

    // (C) 適用
    apply_plan(plan, merged_map.as_ref(), update_id, workspace, recorder).await;
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

/// ファイル末尾へ新規キャラブロックを全属性まとめて追記する。
/// 先頭セクションは `append_new_character`、以降は `append_attribute_section` を連鎖させる
/// (逐次適用と同じ出力になる)。
fn append_new_character_block(
    file_text: &str,
    char_name: &str,
    sections: &[(&'static str, String)],
) -> String {
    let mut iter = sections.iter();
    let Some((heading, body)) = iter.next() else {
        return file_text.to_string();
    };
    let mut out = append_new_character(file_text, char_name, heading, body);
    for (heading, body) in iter {
        match append_attribute_section(&out, char_name, heading, body) {
            Some(c) => out = c,
            None => warn!(
                "character_updater: failed to append section {} to new character {}",
                heading, char_name
            ),
        }
    }
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
/// - `sections` の全属性を1ファイルにまとめて書き出す(同一 run 内の順序依存を排除)。
async fn create_character_file(
    chars_dir: &Path,
    name: &str,
    sections: &[(&'static str, String)],
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
    let mut content = format!("# {}\n", name);
    for (heading, body) in sections {
        content.push_str(&format!("\n## {}\n\n{}\n", heading, body.trim_end()));
    }
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
    fn test_build_batch_merge_prompt_requests_meaningful_merge() {
        let groups = vec![CharMergeGroup {
            file: PathBuf::from("characters.md"),
            display_name: "ジェフ".to_string(),
            sections: vec![SectionMergeTask {
                heading: "背景".to_string(),
                old_body: "- 予備役上がり。".to_string(),
                new_body: "- 元警備隊員。".to_string(),
            }],
        }];
        let prompt =
            build_batch_merge_prompt(&groups).expect("prompt_semantic_merge_batch.md should load");
        assert!(prompt.contains("意味が自然につながるようにマージ"));
        assert!(prompt.contains("両方を機械的に並べず"));
        assert!(prompt.contains("矛盾しない表現へ合成"));
        assert!(prompt.contains("## キャラクター: ジェフ"));
        assert!(prompt.contains("### セクション: 背景"));
        assert!(prompt.contains("- 予備役上がり。"));
        assert!(prompt.contains("- 元警備隊員。"));
        assert!(
            prompt.contains("そのまま返すこと"),
            "name/heading の復唱指示が含まれること"
        );
    }

    #[test]
    fn test_render_char_group_lists_all_sections() {
        let group = CharMergeGroup {
            file: PathBuf::from("characters.md"),
            display_name: "ジェフ".to_string(),
            sections: vec![
                SectionMergeTask {
                    heading: "性格".to_string(),
                    old_body: "- 落ち着いている。".to_string(),
                    new_body: "- 冷静。".to_string(),
                },
                SectionMergeTask {
                    heading: "背景".to_string(),
                    old_body: "- 予備役上がり。".to_string(),
                    new_body: "- 元警備隊員。".to_string(),
                },
            ],
        };
        let out = render_char_group(&group);
        assert!(out.starts_with("## キャラクター: ジェフ"));
        assert!(out.contains("### セクション: 性格"));
        assert!(out.contains("### セクション: 背景"));
        assert!(out.contains("#### 現在の設定\n- 落ち着いている。"));
        assert!(out.contains("#### 新しく本文から読み取れた情報\n- 冷静。"));
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
    fn test_extract_batch_merges_nested_json() {
        let raw = r#"{"characters": [
            {"name": "ジェフ", "sections": [
                {"heading": "性格", "merged_text": "落ち着いた性格。"},
                {"heading": "背景", "merged_text": "- 予備役上がり。"}]},
            {"name": "シルビア", "sections": [
                {"heading": "口調", "merged_text": "丁寧語。"}]}]}"#;
        let merged = extract_batch_merges(raw);
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged[&("ジェフ".to_string(), "性格".to_string())],
            "落ち着いた性格。"
        );
        assert_eq!(
            merged[&("シルビア".to_string(), "口調".to_string())],
            "丁寧語。"
        );
    }

    #[test]
    fn test_extract_batch_merges_strips_fence_and_preamble() {
        // merged_text 内のコードフェンス・前置きラベルは要素単位で除去される
        let raw = r#"{"characters": [{"name": "ジェフ", "sections": [
            {"heading": "性格", "merged_text": "マージ後の文章：\n落ち着いた性格。"}]}]}"#;
        let merged = extract_batch_merges(raw);
        assert_eq!(
            merged[&("ジェフ".to_string(), "性格".to_string())],
            "落ち着いた性格。"
        );
    }

    #[test]
    fn test_extract_batch_merges_non_json_returns_empty() {
        assert!(extract_batch_merges("これはJSONではない").is_empty());
        // 旧単発形式は受理しない(全滅としてリトライに回る)
        assert!(extract_batch_merges(r#"{"merged_text": "旧形式"}"#).is_empty());
    }

    #[test]
    fn test_lookup_merged_exact_and_normalized_fallback() {
        let mut m = HashMap::new();
        m.insert(("ジェフ".to_string(), "性格".to_string()), "A".to_string());
        // 完全一致
        assert_eq!(
            lookup_merged(&m, "ジェフ", "性格", &CharacterAttribute::Personality),
            Some("A")
        );
        // echo が縮んだケース: 期待見出しは「性格・口調」だが応答は「性格」→ 属性正規化で一意に一致
        assert_eq!(
            lookup_merged(&m, "ジェフ", "性格・口調", &CharacterAttribute::Personality),
            Some("A")
        );
        // 名前不一致は不採用(old 維持)
        assert_eq!(
            lookup_merged(&m, "シルビア", "性格", &CharacterAttribute::Personality),
            None
        );
    }

    #[test]
    fn test_lookup_merged_ambiguous_returns_none() {
        let mut m = HashMap::new();
        m.insert(("ジェフ".to_string(), "性格".to_string()), "A".to_string());
        m.insert(
            ("ジェフ".to_string(), "性格・口調".to_string()),
            "B".to_string(),
        );
        // 「気質」(Personality へ正規化)で引くと両方が候補 → 曖昧なので採用しない
        assert_eq!(
            lookup_merged(&m, "ジェフ", "気質", &CharacterAttribute::Personality),
            None
        );
    }

    // ---- run_batch_merge のテスト ----

    /// テスト専用の最小 LlmInterface 実装。あらかじめ積んだ応答列を `chat()` 呼び出しごとに
    /// 1つずつ返す(FIFO)。応答が尽きたら空文字を返す。
    #[derive(Debug)]
    struct FakeLlmClient {
        responses: std::collections::VecDeque<String>,
        call_count: usize,
    }

    impl FakeLlmClient {
        fn with_responses(responses: &[&str]) -> Self {
            Self {
                responses: responses.iter().map(|s| s.to_string()).collect(),
                call_count: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmInterface for FakeLlmClient {
        async fn chat(&mut self) -> Result<String, crate::llm::LlmError> {
            self.call_count += 1;
            Ok(self.responses.pop_front().unwrap_or_default())
        }
        async fn with_model(&mut self, _model: &str) -> Result<String, crate::llm::LlmError> {
            Ok(String::new())
        }
        fn add(&mut self, _prompt: Content) {}
        fn build_content(&self) -> String {
            String::new()
        }
        fn get_model(&self) -> &str {
            "fake-model"
        }
        fn clear(&mut self) {}
        fn cache(&mut self, _prompt: Content) -> Result<String, crate::llm::LlmError> {
            Ok(String::new())
        }
        fn fetch(&self, _hash: &str) -> Option<&Content> {
            None
        }
        fn fetch_all(&self) -> Vec<Content> {
            Vec::new()
        }
        fn remove(&mut self, _hash: String) {}
        fn capabilities(&self) -> ModelCapability {
            ModelCapability::STRUCTURED_OUTPUT
        }
        fn max_tokens(&mut self, _n: u32) {}
        fn temperature(&mut self, _v: f64) {}
        fn top_p(&mut self, _v: f64) {}
        fn stop_sequences(&mut self, _seqs: Vec<String>) {}
        fn seed(&mut self, _v: u64) {}
        fn reasoning_effort(&mut self, _effort: genai::chat::ReasoningEffort) {}
        fn reasoning_level(&mut self, _level: f64) {}
        fn response_format(&mut self, _fmt: ChatResponseFormat) {}
        fn service_tier(&mut self, _tier: genai::chat::ServiceTier) {}
        fn verbosity(&mut self, _v: genai::chat::Verbosity) {}
        fn add_tool(&mut self, _tool: Box<dyn crate::llm::LlmTool>) {}
        async fn respond_tool(
            &self,
            _tools: &[genai::chat::ToolCall],
        ) -> Result<genai::chat::ChatRequest, crate::llm::LlmError> {
            Err(crate::llm::LlmError::NotImplemented)
        }
    }

    fn one_group() -> Vec<CharMergeGroup> {
        vec![CharMergeGroup {
            file: PathBuf::from("characters.md"),
            display_name: "ジェフ".to_string(),
            sections: vec![SectionMergeTask {
                heading: "性格".to_string(),
                old_body: "- 落ち着いている。".to_string(),
                new_body: "- 冷静。".to_string(),
            }],
        }]
    }

    #[tokio::test]
    async fn test_run_batch_merge_succeeds_on_first_response() {
        let groups = one_group();
        let mut fake = FakeLlmClient::with_responses(&[
            r#"{"characters": [{"name": "ジェフ", "sections": [{"heading": "性格", "merged_text": "冷静で落ち着いている。"}]}]}"#,
        ]);
        let merged = run_batch_merge(&mut fake, &groups).await.unwrap();
        assert_eq!(fake.call_count, 1, "1回で成功すればリトライしないこと");
        assert_eq!(
            merged[&("ジェフ".to_string(), "性格".to_string())],
            "冷静で落ち着いている。"
        );
    }

    #[tokio::test]
    async fn test_run_batch_merge_retries_once_on_total_miss() {
        let groups = one_group();
        let mut fake = FakeLlmClient::with_responses(&[
            "これはスキーマに適合しない応答です",
            r#"{"characters": [{"name": "ジェフ", "sections": [{"heading": "性格", "merged_text": "冷静で落ち着いている。"}]}]}"#,
        ]);
        let merged = run_batch_merge(&mut fake, &groups).await.unwrap();
        assert_eq!(fake.call_count, 2, "全滅時は1回だけリトライすること");
        assert_eq!(
            merged[&("ジェフ".to_string(), "性格".to_string())],
            "冷静で落ち着いている。"
        );
    }

    #[tokio::test]
    async fn test_run_batch_merge_no_retry_on_partial_hit() {
        // 2キャラのうち1件しか返らなくても、部分欠落は全体リトライしない
        // (欠落側は呼び出し側=apply_plan で old 維持にフォールバックする)。
        let groups = vec![
            CharMergeGroup {
                file: PathBuf::from("characters.md"),
                display_name: "ジェフ".to_string(),
                sections: vec![SectionMergeTask {
                    heading: "性格".to_string(),
                    old_body: "- 落ち着いている。".to_string(),
                    new_body: "- 冷静。".to_string(),
                }],
            },
            CharMergeGroup {
                file: PathBuf::from("characters.md"),
                display_name: "シルビア".to_string(),
                sections: vec![SectionMergeTask {
                    heading: "口調".to_string(),
                    old_body: "- 丁寧語。".to_string(),
                    new_body: "- 敬語。".to_string(),
                }],
            },
        ];
        let mut fake = FakeLlmClient::with_responses(&[
            r#"{"characters": [{"name": "ジェフ", "sections": [{"heading": "性格", "merged_text": "冷静で落ち着いている。"}]}]}"#,
        ]);
        let merged = run_batch_merge(&mut fake, &groups).await.unwrap();
        assert_eq!(fake.call_count, 1, "部分欠落では全体リトライしないこと");
        assert!(merged.contains_key(&("ジェフ".to_string(), "性格".to_string())));
        assert!(
            !merged.contains_key(&("シルビア".to_string(), "口調".to_string())),
            "欠落分はマップに無いこと(apply_plan 側で old 維持)"
        );
    }

    // ---- plan_updates のテスト ----

    const PLAN_MD: &str = "\
# ジェフ

## 性格・口調

- 落ち着いている。

## 呼称

- クライン艦長

# シルビア

## 性格

- 真面目。
";

    fn plan_input(items: &[(&str, &str, &str)]) -> Vec<UpdateItem> {
        items
            .iter()
            .map(|(n, a, t)| UpdateItem {
                name: n.to_string(),
                attribute: a.to_string(),
                text: t.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_plan_updates_merges_attribute_synonyms_into_one_task() {
        let file = PathBuf::from("characters.md");
        let snapshots = HashMap::from([(file.clone(), PLAN_MD.to_string())]);
        let updates = plan_input(&[
            ("ジェフ", "性格", "- 冷静。"),
            ("ジェフ", "personality", "- 短気な一面。"),
        ]);
        let (plan, groups) = plan_updates(&updates, &[file], &snapshots);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].sections.len(),
            1,
            "属性シノニムは1タスクへ合流すること"
        );
        assert_eq!(groups[0].sections[0].new_body, "- 冷静。\n- 短気な一面。");
        let merges: Vec<_> = plan
            .iter()
            .filter(|op| matches!(op, ResolvedOp::Merge { .. }))
            .collect();
        assert_eq!(merges.len(), 1, "Merge op は1宛先1つであること");
        if let ResolvedOp::Merge { records, .. } = merges[0] {
            assert_eq!(records.len(), 2, "合流しても両 item の記録が残ること");
        }
    }

    #[test]
    fn test_plan_updates_merges_multi_tag_heading_into_one_task() {
        let file = PathBuf::from("characters.md");
        let snapshots = HashMap::from([(file.clone(), PLAN_MD.to_string())]);
        let updates = plan_input(&[
            ("ジェフ", "性格", "- 冷静。"),
            ("ジェフ", "口調", "- ぶっきらぼう。"),
        ]);
        let (_plan, groups) = plan_updates(&updates, &[file], &snapshots);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].sections.len(),
            1,
            "多タグ見出し(性格・口調)への2属性は同一物理セクションの1タスクへ合流すること"
        );
        assert!(groups[0].sections[0].new_body.contains("- 冷静。"));
        assert!(groups[0].sections[0].new_body.contains("- ぶっきらぼう。"));
        assert_eq!(groups[0].sections[0].heading, "性格・口調");
    }

    #[test]
    fn test_plan_updates_groups_by_character() {
        let file = PathBuf::from("characters.md");
        let snapshots = HashMap::from([(file.clone(), PLAN_MD.to_string())]);
        let updates = plan_input(&[
            ("ジェフ", "性格", "- 冷静。"),
            ("シルビア", "性格", "- 几帳面。"),
        ]);
        let (_plan, groups) = plan_updates(&updates, &[file], &snapshots);
        assert_eq!(groups.len(), 2, "キャラごとに別グループになること");
        let names: Vec<_> = groups.iter().map(|g| g.display_name.as_str()).collect();
        assert!(names.contains(&"ジェフ"));
        assert!(names.contains(&"シルビア"));
    }

    #[test]
    fn test_plan_updates_alias_is_deterministic_not_batched() {
        let file = PathBuf::from("characters.md");
        let snapshots = HashMap::from([(file.clone(), PLAN_MD.to_string())]);
        let updates = plan_input(&[("ジェフ", "呼称", "艦長")]);
        let (plan, groups) = plan_updates(&updates, &[file], &snapshots);
        assert!(groups.is_empty(), "Alias は LLM バッチに載せないこと");
        assert!(
            plan.iter()
                .any(|op| matches!(op, ResolvedOp::ReplaceAlias { .. })),
            "決定的マージの ReplaceAlias になること"
        );
    }

    #[test]
    fn test_plan_updates_aggregates_new_character_into_single_create() {
        // 対応ファイルが無い名前 → CreateFile 1つに全属性を集約(順序依存の排除)
        let updates = plan_input(&[
            ("新キャラ", "性格", "- 明るい。"),
            ("新キャラ", "背景", "- 謎の過去。"),
        ]);
        let (plan, groups) = plan_updates(&updates, &[], &HashMap::new());
        assert!(groups.is_empty(), "新規キャラは LLM マージ対象にならないこと");
        let creates: Vec<_> = plan
            .iter()
            .filter(|op| matches!(op, ResolvedOp::CreateFile { .. }))
            .collect();
        assert_eq!(creates.len(), 1, "同一新規キャラは1つの CreateFile に集約されること");
        if let ResolvedOp::CreateFile { sections, records, .. } = creates[0] {
            assert_eq!(sections.len(), 2);
            assert_eq!(records.len(), 2);
        }
    }

    #[tokio::test]
    async fn test_apply_plan_missing_merge_keeps_old() {
        // バッチ結果から対象キーが欠落 → old 維持(ファイルを書き換えない)
        let dir = std::env::temp_dir().join("ff_batch_merge_missing_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("characters.md");
        std::fs::write(&file, PLAN_MD).unwrap();

        let snapshots = HashMap::from([(file.clone(), PLAN_MD.to_string())]);
        let updates = plan_input(&[("ジェフ", "性格", "- 冷静。")]);
        let (plan, groups) = plan_updates(&updates, std::slice::from_ref(&file), &snapshots);
        assert_eq!(groups.len(), 1);

        let recorder = FlightRecorder::new(&dir.join("flight.db"));
        let update_id = recorder.record_character_update("test://uri", "test-model", "prompt");
        apply_plan(plan, Some(&HashMap::new()), update_id, &dir, &recorder).await;

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            PLAN_MD,
            "マージ結果欠落時はファイルが書き換えられないこと"
        );
        drop(recorder);
        let _ = std::fs::remove_dir_all(&dir);
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
