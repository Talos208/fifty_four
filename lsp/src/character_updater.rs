use crate::llm::{Content, LlmClient, ModelCapability};
use crate::{parse_all_content, CharacterAttribute, FlightRecorder};
use dashmap::DashMap;
use genai::chat::{ChatResponseFormat, JsonSpec};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::WorkspaceFolder;

use crate::types::LineData;

const IDLE_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
pub const MIN_CHARS: usize = 1000;

/// URI ごとの更新トリガー状態
#[derive(Debug)]
pub struct UpdateState {
    pub last_change_at: Instant,
    pub first_dirty_at: Option<Instant>,
    pub dirty_lines: BTreeSet<usize>,
    pub accumulated_chars: usize,
    pub pending_task: Option<JoinHandle<()>>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            last_change_at: Instant::now(),
            first_dirty_at: None,
            dirty_lines: BTreeSet::new(),
            accumulated_chars: 0,
            pending_task: None,
        }
    }
}

impl UpdateState {
    pub fn reset(&mut self) {
        self.dirty_lines.clear();
        self.accumulated_chars = 0;
        self.first_dirty_at = None;
        self.pending_task = None;
    }
}

/// `first_dirty_at` からの経過時間を踏まえて待機 Duration を計算する。
/// idle 3分 と max 待ち残り時間のうち短い方を返す。
pub fn compute_wait(first_dirty_at: Instant) -> Duration {
    let elapsed = first_dirty_at.elapsed();
    let max_remaining = MAX_WAIT.saturating_sub(elapsed);
    IDLE_TIMEOUT.min(max_remaining)
}


/// `dirty_lines` の行番号集合から最新テキストを抽出する。
/// 連続する行は "\n" で結合し、離れた区間は "\n\n" で区切る。
pub fn dirty_lines_to_text(
    text: &DashMap<String, Vec<LineData>>,
    uri: &str,
    dirty_lines: &BTreeSet<usize>,
) -> String {
    if dirty_lines.is_empty() {
        return String::new();
    }

    let Some(lines) = text.get(uri) else {
        return String::new();
    };

    let mut result = String::new();
    let mut prev: Option<usize> = None;

    for &ln in dirty_lines {
        if let Some(p) = prev {
            if ln == p + 1 {
                result.push('\n');
            } else {
                result.push_str("\n\n");
            }
        }
        if let Some(line_data) = lines.get(ln) {
            result.push_str(&line_data.text);
        }
        prev = Some(ln);
    }

    result
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

/// did_change から spawn される one-shot 遅延タスク。
/// `wait` だけ sleep してから `run` を呼び出す。ループなし。
pub async fn run_at(
    wait: Duration,
    uri: String,
    workspace_arc: Arc<TokioMutex<Vec<WorkspaceFolder>>>,
    text: DashMap<String, Vec<LineData>>,
    llm: Arc<TokioMutex<Option<Box<dyn LlmClient>>>>,
    recorder: Arc<FlightRecorder>,
    state: Arc<parking_lot::Mutex<UpdateState>>,
) {
    tokio::time::sleep(wait).await;

    let workspace_path = {
        let ws = workspace_arc.lock().await;
        ws.first()
            .and_then(|w| w.uri.to_file_path().ok())
            .unwrap_or_default()
    };

    let extracted = {
        let s = state.lock();
        dirty_lines_to_text(&text, &uri, &s.dirty_lines)
    };

    if extracted.is_empty() {
        state.lock().reset();
        return;
    }

    run(uri, workspace_path, extracted, llm, recorder, state).await;
}

/// キャラクター更新タスクの本体。
pub async fn run(
    uri: String,
    workspace: PathBuf,
    extracted_text: String,
    llm: Arc<TokioMutex<Option<Box<dyn LlmClient>>>>,
    recorder: Arc<FlightRecorder>,
    state: Arc<parking_lot::Mutex<UpdateState>>,
) {
    debug!("character_updater::run start for {}", uri);

    // 1. characters/*.md を全て読み込んでキャラ一覧を構築
    let char_files = collect_character_files(&workspace).await;
    if char_files.is_empty() {
        debug!("character_updater: no character files found");
        state.lock().reset();
        return;
    }

    // キャラ一覧テキストを構築
    let known_chars_text = build_known_chars_summary(&char_files).await;

    // 2. プロンプトを組み立て
    let prompt_template = match crate::Asset::get("prompt_character_update.md") {
        Some(f) => String::from_utf8_lossy(f.data.as_ref()).to_string(),
        None => {
            error!("prompt_character_update.md not found");
            state.lock().reset();
            return;
        }
    };

    // frontmatter を除去してプロンプト本文のみ取得
    let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
    let parsed = matter.parse::<HashMap<String, String>>(prompt_template.as_str());
    let (prompt_body, frontmatter_data) = match parsed {
        Ok(p) => (p.content, p.data.unwrap_or_default()),
        Err(_) => (prompt_template.clone(), HashMap::default()),
    };

    let prompt = prompt_body
        .replace("{{KNOWN_CHARACTERS}}", &known_chars_text)
        .replace("{{TEXT}}", &extracted_text);

    // 3. LLM 呼び出し
    let schema_str = frontmatter_data
        .get("schema")
        .cloned()
        .unwrap_or_default();
    let schema: Value = match serde_json::from_str(&schema_str) {
        Ok(v) => v,
        Err(e) => {
            error!("character_updater: invalid schema: {}", e);
            state.lock().reset();
            return;
        }
    };

    let model_name;
    let raw_response;
    {
        let mut ref_llm = llm.lock().await;
        let Some(llm_client) = ref_llm.as_mut() else {
            debug!("character_updater: LLM not initialized");
            state.lock().reset();
            return;
        };
        model_name = llm_client.get_model().to_string();

        let update_id = recorder.record_character_update(&uri, &model_name, &prompt);

        if llm_client
            .capabilities()
            .contains(ModelCapability::STRUCTURED_OUTPUT)
        {
            llm_client.response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
                "character_updates",
                schema,
            )));
        } else {
            llm_client.add(Content::Text(format!(
                "回答は次のJSON schemaに厳密にしたがって生成せよ。\n\nJSON Schema:\n{}\n\n最終応答はスキーマに適合するJSONのみを出力し、JSON以外の文字は一切含めないこと。",
                schema_str
            )));
        }
        llm_client.add(Content::Text(prompt.clone()));

        raw_response = llm_client.chat().await;

        match &raw_response {
            Ok(resp) => {
                recorder.record_character_response(update_id, resp);
                // 4. JSON をパースして差し替え
                apply_updates(resp, update_id, &char_files, &recorder).await;
                recorder.complete_character_update(update_id);
            }
            Err(e) => {
                error!("character_updater: LLM error: {}", e);
                recorder.record_character_response(update_id, &format!("ERROR: {}", e));
            }
        }
    }

    state.lock().reset();
    debug!("character_updater::run done for {}", uri);
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

/// キャラ設定ファイルのサマリーテキストを構築する(LLMに既知キャラを伝えるため)。
async fn build_known_chars_summary(char_files: &[PathBuf]) -> String {
    let mut summary = String::new();
    for path in char_files {
        let Ok(content) = tokio::fs::read_to_string(path).await else {
            continue;
        };
        let chars = parse_all_content(&content);
        for (name, entry) in &chars {
            summary.push_str(&format!("## {}\n", name));
            for section in &entry.sections {
                let attr_str = section
                    .tags
                    .iter()
                    .map(|t| format!("{:?}", t).to_lowercase())
                    .collect::<Vec<_>>()
                    .join("・");
                summary.push_str(&format!("### {}\n{}\n\n", attr_str, section.text.trim()));
            }
        }
    }
    summary
}

/// LLM レスポンス JSON を解析して characters/*.md に差し替えを適用する。
async fn apply_updates(
    response: &str,
    update_id: i64,
    char_files: &[PathBuf],
    recorder: &FlightRecorder,
) {
    let parsed: Value = match serde_json::from_str(response) {
        Ok(v) => v,
        Err(e) => {
            error!("character_updater: failed to parse response JSON: {}", e);
            return;
        }
    };

    let updates = match parsed.get("updates").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            debug!("character_updater: no updates in response");
            return;
        }
    };

    for item in updates {
        let name = match item.get("name").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let attr_str = match item.get("attribute").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let new_text = match item.get("text").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };

        let attr = match CharacterAttribute::try_from(attr_str) {
            Ok(a) => a,
            Err(_) => {
                recorder.record_character_section(
                    update_id,
                    name,
                    attr_str,
                    None,
                    new_text,
                    false,
                    Some("unknown attribute"),
                );
                continue;
            }
        };

        // 対応ファイルを探す
        let target_file = find_character_file(char_files, name);
        let Some(file_path) = target_file else {
            recorder.record_character_section(
                update_id,
                name,
                attr_str,
                None,
                new_text,
                false,
                Some("character file not found"),
            );
            continue;
        };

        let Ok(file_content) = tokio::fs::read_to_string(&file_path).await else {
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
        };

        // 既存のセクション本文を取得して変更チェック
        let old_text = extract_section_text(&file_content, name, &attr);

        if old_text.as_deref() == Some(new_text.trim()) {
            recorder.record_character_section(
                update_id,
                name,
                attr_str,
                old_text.as_deref(),
                new_text,
                false,
                Some("no change"),
            );
            continue;
        }

        match replace_section(&file_content, name, &attr, new_text) {
            Some(new_content) => {
                match tokio::fs::write(&file_path, new_content).await {
                    Ok(_) => {
                        info!("character_updater: updated {}/{}", name, attr_str);
                        recorder.record_character_section(
                            update_id,
                            name,
                            attr_str,
                            old_text.as_deref(),
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
                            old_text.as_deref(),
                            new_text,
                            false,
                            Some("write failed"),
                        );
                    }
                }
            }
            None => {
                recorder.record_character_section(
                    update_id,
                    name,
                    attr_str,
                    old_text.as_deref(),
                    new_text,
                    false,
                    Some("section not found in file"),
                );
            }
        }
    }
}

/// ファイル一覧からキャラ名を前方一致で探す。
fn find_character_file<'a>(files: &'a [PathBuf], name: &str) -> Option<&'a PathBuf> {
    files.iter().find(|p| {
        p.file_stem()
            .map(|s| s.to_string_lossy().starts_with(name))
            .unwrap_or(false)
    })
}

/// ファイルテキストから指定キャラ・属性のセクション本文を抽出する。
fn extract_section_text(file_text: &str, char_name: &str, attribute: &CharacterAttribute) -> Option<String> {
    let chars = parse_all_content(file_text);
    let (_, entry) = chars.iter().find(|(k, _)| k.contains(char_name))?;
    let section = entry
        .sections
        .iter()
        .find(|s| s.tags.contains(attribute))?;
    Some(section.text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LineData;
    use std::str::FromStr;

    fn make_text(uri: &str, content: &str) -> DashMap<String, Vec<LineData>> {
        let map = DashMap::new();
        let lines = content
            .lines()
            .map(|l| LineData::from_str(l).unwrap())
            .collect();
        map.insert(uri.to_string(), lines);
        map
    }

    #[test]
    fn test_dirty_lines_to_text_continuous() {
        let text = make_text("file.txt", "line0\nline1\nline2\nline3");
        let mut dirty = BTreeSet::new();
        dirty.insert(1);
        dirty.insert(2);
        let result = dirty_lines_to_text(&text, "file.txt", &dirty);
        assert_eq!(result, "line1\nline2");
    }

    #[test]
    fn test_dirty_lines_to_text_gap() {
        let text = make_text("file.txt", "line0\nline1\nline2\nline3\nline4");
        let mut dirty = BTreeSet::new();
        dirty.insert(1);
        dirty.insert(3);
        let result = dirty_lines_to_text(&text, "file.txt", &dirty);
        assert_eq!(result, "line1\n\nline3");
    }

    #[test]
    fn test_compute_wait_idle_dominates() {
        // first_dirty が 5分前 → max残り 5min > idle 3min → wait = 3min
        let first_dirty = Instant::now() - Duration::from_secs(5 * 60);
        assert_eq!(compute_wait(first_dirty), IDLE_TIMEOUT);
    }

    #[test]
    fn test_compute_wait_max_dominates() {
        // first_dirty が 9分前 → max残り 1min < idle 3min → wait = 1min
        let first_dirty = Instant::now() - Duration::from_secs(9 * 60);
        let wait = compute_wait(first_dirty);
        assert!(wait < IDLE_TIMEOUT, "max wait 側が使われるはず");
        assert!(wait <= Duration::from_secs(60 + 1), "1分以下のはず");
    }

    #[test]
    fn test_compute_wait_expired() {
        // first_dirty が 11分前 → max残り 0 → 即時(Duration::ZERO)
        let first_dirty = Instant::now() - Duration::from_secs(11 * 60);
        assert_eq!(compute_wait(first_dirty), Duration::ZERO);
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
            "- 元警備隊員で予備役上がり。\n- フォン・ブラウン出身。",
        );
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(out.contains("元警備隊員で予備役上がり"));
        assert!(!out.contains("- 予備役上がり。\n"), "旧テキストが残ってはいけない");
        assert!(out.contains("落ち着いている"), "他セクションが保持されること");
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
            "- 真面目で緊張しやすい。",
        );
        assert!(result.is_some());
        let out = result.unwrap();
        assert!(out.contains("落ち着いている"), "ジェフのセクションが保持されること");
        assert!(out.contains("真面目で緊張しやすい"), "更新後のテキストがあること");
        assert!(!out.contains("- 真面目。\n"), "旧テキストがないこと");
    }
}
