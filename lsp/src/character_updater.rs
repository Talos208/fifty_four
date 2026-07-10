use crate::llm::{Content, LlmInterface, ModelCapability};
use crate::{CharacterAttribute, FlightRecorder, parse_all_content, shorten_middle};
use dashmap::DashMap;
use genai::chat::{ChatResponseFormat, JsonSpec};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tower_lsp::lsp_types::WorkspaceFolder;

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
    llm_client.max_tokens(1024);
    llm_client.reasoning_level(0.0);
    llm_client.add(Content::Text(prompt));
    let merged = sanitize_merged_section_text(&llm_client.chat().await?);

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
    workspace_arc: Arc<TokioMutex<Vec<WorkspaceFolder>>>,
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
        ws.first()
            .and_then(|w| w.uri.to_file_path().ok())
            .unwrap_or_default()
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
    let raw_response;
    {
        let mut ref_llm = llm.lock().await;
        let Some(llm_client) = ref_llm.as_mut() else {
            debug!("character_updater: LLM not initialized");
            return;
        };
        model_name = llm_client.get_model().to_string();
        debug!("character_updater::run: calling LLM model={}", model_name);

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
        llm_client.reasoning_level(0.8); // 重要なので考える

        raw_response = llm_client.chat().await;

        match &raw_response {
            Ok(resp) => {
                debug!("character_updater::run: LLM response received :{}", resp);
                recorder.record_character_response(update_id, resp);
                // 4. JSON をパースして差し替え・追記・新規作成
                apply_updates(
                    resp,
                    update_id,
                    char_files,
                    &workspace,
                    &recorder,
                    llm_client.as_mut(),
                )
                .await;
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

/// LLM レスポンス JSON を解析して characters/*.md に差し替え・追記・新規作成を適用する。
/// `char_files` は値で受け取り、新規作成したファイルを push する
/// (同一レスポンス内の後続 item が同じキャラの別属性を追記できるよう)。
async fn apply_updates(
    response: &str,
    update_id: i64,
    mut char_files: Vec<PathBuf>,
    workspace: &Path,
    recorder: &FlightRecorder,
    llm_client: &mut dyn LlmInterface,
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
    debug!("character_updater: {} update(s) received", updates.len());

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

        debug!(
            "character_updater: processing update name={:?} attribute={:?} text={}",
            name,
            attr_str,
            shorten_middle(new_text, 40)
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
                    new_text,
                    false,
                    Some("unknown attribute"),
                );
                continue;
            }
        };

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
            let char_entry = chars.iter().find(|(k, _)| k.contains(name));

            let (old_text_opt, new_content_opt): (Option<String>, Option<String>) = match char_entry
            {
                Some((_, entry)) => {
                    // キャラ見出しが存在する
                    if let Some(sec) = entry.sections.iter().find(|s| s.tags.contains(&attr)) {
                        // 既存属性セクション → LLM で意味的にマージ
                        let old = sec.text.trim().to_string();
                        let merged = match merge_section_text_semantically(
                            llm_client, name, &attr, &old, new_text,
                        )
                        .await
                        {
                            Ok(Some(merged)) => merged,
                            Ok(None) => {
                                debug!(
                                    "character_updater: {}/{} no semantic change, skip",
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
                        match replace_section(&file_content, name, &attr, &merged) {
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
                        // 属性セクションが未存在 → 追記
                        match append_attribute_section(
                            &file_content,
                            name,
                            attr.canonical_heading(),
                            new_text,
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
                    // 単一ファイル形式の新規キャラ → キャラブロックを末尾追記
                    let c = append_new_character(
                        &file_content,
                        name,
                        attr.canonical_heading(),
                        new_text,
                    );
                    (None, Some(c))
                }
            };

            if let Some(content) = new_content_opt {
                match tokio::fs::write(&file_path, &content).await {
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
            // ---- ファイルが見つからない → フォルダ形式の新規キャラ ----
            let chars_dir = workspace.join("characters");
            match create_character_file(&chars_dir, name, attr.canonical_heading(), new_text).await
            {
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
    match tokio::fs::write(&path, &content).await {
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
}
