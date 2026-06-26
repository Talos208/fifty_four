# PlotInfoTool 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `plot.md` の章ごとのプロット情報を completion 時に LLM へ tool call 経由で提供し、あわせて `CharacterInfoTool` を `tools.rs` に集約する。

**Architecture:** `LlmTool` トレイトを実装した `PlotInfoTool` を新規 `lsp/src/tools.rs` に追加する。既存の `CharacterInfoTool` も同ファイルに移動する。`completion()` の `use_llm_with_option` クロージャで両ツールを `add_tool` する。

**Tech Stack:** Rust, tokio（async fs）, serde_json, async-trait, parking_lot

## Global Constraints

- `cargo build` がエラーなしで通ること
- `cargo test` で既存テストが全てパスすること
- 新規ファイル: `lsp/src/tools.rs`
- `plot.md` はワークスペースルートに配置: `<workspace>/plot.md`
- キャッシュ機構は初期実装では持たない（毎回ファイル読み込み）

---

### Task 1: tools.rs を作成し CharacterInfoTool を移動する

**Files:**
- Create: `lsp/src/tools.rs`
- Modify: `lsp/src/main.rs`

**Interfaces:**
- Produces: `pub(crate) fn CharacterInfoTool::new(workspace: &Path, cache: CharacterCache) -> Box<dyn LlmTool>`

- [ ] **Step 1: main.rs の型定義を pub(crate) に昇格させる**

`lsp/src/main.rs` で以下の型と各フィールドを `pub(crate)` にする（`tools.rs` から参照するため）。

```rust
// TaggedContent（元: line 349 あたり）
#[derive(Debug, Clone)]
pub(crate) struct TaggedContent {
    pub(crate) tags: Vec<CharacterAttribute>,
    pub(crate) text: String,
}

// CharacterEntry（元: line 358 あたり）
#[derive(Debug, Clone)]
pub(crate) struct CharacterEntry {
    pub(crate) sections: Vec<TaggedContent>,
}

// FileCacheEntry（元: line 365 あたり）
#[derive(Debug)]
pub(crate) struct FileCacheEntry {
    pub(crate) modified: std::time::SystemTime,
    pub(crate) characters: HashMap<String, CharacterEntry>,
}

// CharacterCache（元: line 372 あたり）
#[derive(Debug, Clone)]
pub(crate) struct CharacterCache(pub(crate) Arc<parking_lot::Mutex<HashMap<PathBuf, FileCacheEntry>>>);

// shorten_middle（元: line 748 あたり）
pub(crate) fn shorten_middle(s: &str, len: usize) -> String {
    // 既存実装のまま
```

- [ ] **Step 2: tools.rs を新規作成して CharacterInfoTool を貼り付ける**

`lsp/src/tools.rs` を新規作成し、以下の内容を記述する（`CharacterInfoTool` の実装は `main.rs` の既存コードをそのまま移動）。

```rust
use crate::llm::{LlmError, LlmTool};
use crate::{
    CharacterAttribute, CharacterCache, FileCacheEntry, find_character_file_path,
    parse_all_content, shorten_middle,
};
use async_trait::async_trait;
#[allow(unused_imports)]
use log::debug;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

// ─── CharacterInfoTool ───────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct CharacterInfoTool {
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
                        "type": "string",
                        "enum": ["role", "appearance", "personality", "expression",
                                 "background", "relationship", "weakness", "style"],
                    },
                    "minItems": 1,
                    "description": "取得したい属性のタグ"
                }
            },
            "required": ["character_name", "tags"],
        })
    }

    fn name(&self) -> &str { "character_info" }
    fn description(&self) -> &str { "キャラクターの設定を取得する" }

    async fn invoke(
        &self,
        _args: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, LlmError> {
        let name = _args["character_name"].as_str().unwrap_or("");
        let tags: Vec<String> = _args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        debug!("CharacterInfoTool({}, {:?})", name, tags);

        let path = self.find_character_file_path(name)?;
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        {
            let cache = self.cache.0.lock();
            if let Some(entry) = cache.get(&path)
                && entry.modified == modified
            {
                let result = Self::search_cache(entry, name, &tags);
                debug!("\t{:?}", result.map(|r| shorten_middle(&r, 40)))
            }
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| LlmError::GenericError {
                message: format!("Failed to read {:?}: {}", &path, e),
            })?;
        debug!("{}", shorten_middle(&content, 40));

        let characters = parse_all_content(&content);
        let file_entry = FileCacheEntry { modified, characters };
        let result = Self::search_cache(&file_entry, name, &tags);
        self.cache.0.lock().insert(path, file_entry);
        debug!("\t{:?}", result.as_ref().map(|r| shorten_middle(r, 40)));
        result
    }
}

impl CharacterInfoTool {
    pub(crate) fn new(workspace: &Path, cache: CharacterCache) -> Box<dyn LlmTool> {
        Box::new(Self { workspace: workspace.to_path_buf(), cache })
    }

    fn find_character_file_path(&self, name: &str) -> std::result::Result<PathBuf, LlmError> {
        find_character_file_path(&self.workspace, name)
    }

    fn search_cache(
        entry: &FileCacheEntry,
        name: &str,
        tags: &[String],
    ) -> std::result::Result<String, LlmError> {
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
```

- [ ] **Step 3: main.rs から CharacterInfoTool を削除し tools モジュールを追加する**

`main.rs` に対して以下の3点を変更する。

**3-a.** `mod character_updater;` の近くに `mod tools;` を追加する：
```rust
mod character_updater;
mod tools;   // ← 追加
```

**3-b.** `main.rs` 内の `CharacterInfoTool` 定義（struct + impl LlmTool + impl CharacterInfoTool、合計 ~175行）を削除する。

**3-c.** `completion()` 内の `CharacterInfoTool::new(` を `tools::CharacterInfoTool::new(` に変更する（プレフィックス `tools::` を追加するだけ）。

- [ ] **Step 4: ビルドしてテストがパスすることを確認する**

```
cd lsp && cargo test 2>&1 | tail -30
```

期待される出力：
```
test result: ok. N passed; 0 failed; 0 ignored
```

`pub(crate)` が足りない型/フィールドがあればエラーメッセージを見て追加する（パターン: `field ... is private`）。

- [ ] **Step 5: コミットする**

```bash
git add lsp/src/tools.rs lsp/src/main.rs
git commit -m "refactor(lsp): CharacterInfoToolをtools.rsに移動"
```

---

### Task 2: parse_plot_md を TDD で実装する

**Files:**
- Modify: `lsp/src/tools.rs`

**Interfaces:**
- Produces: `pub(crate) fn parse_plot_md(content: &str) -> Vec<(String, String)>`
  - 返り値: `(章名, プロット本文)` のリスト。章名は `# 見出し` のテキスト、本文は trim 済み。
  - level-2 以上の見出し（`##`）は本文として扱う。
  - 最初の見出しより前のコンテンツは無視する。

- [ ] **Step 1: テストを tools.rs 末尾に書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PLOT: &str = "\
# 第1章

主人公が港に到着する。
嵐の予兆。

# 第2章

船が出航する。最初の試練。

# 第3章

敵の船が追いかけてくる。
";

    #[test]
    fn test_parse_plot_md_basic() {
        let chapters = parse_plot_md(SAMPLE_PLOT);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0].0, "第1章");
        assert!(chapters[0].1.contains("主人公が港に到着する"));
        assert!(chapters[0].1.contains("嵐の予兆"));
        assert_eq!(chapters[1].0, "第2章");
        assert_eq!(chapters[2].0, "第3章");
    }

    #[test]
    fn test_parse_plot_md_level2_headings_treated_as_body() {
        let md = "# 第1章\n## サブセクション\n本文。\n# 第2章\n内容。\n";
        let chapters = parse_plot_md(md);
        assert_eq!(chapters.len(), 2);
        assert!(chapters[0].1.contains("本文"), "level-2見出しは本文として扱う");
    }

    #[test]
    fn test_parse_plot_md_trims_body() {
        let md = "# 第1章\n\n  内容  \n\n";
        let chapters = parse_plot_md(md);
        assert_eq!(chapters[0].1, "内容");
    }

    #[test]
    fn test_parse_plot_md_empty_input() {
        assert!(parse_plot_md("").is_empty());
        assert!(parse_plot_md("見出しなしの本文のみ。").is_empty());
    }

    #[test]
    fn test_parse_plot_md_content_before_first_heading_ignored() {
        let md = "前書き。\n# 第1章\n内容。\n";
        let chapters = parse_plot_md(md);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].0, "第1章");
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

```
cd lsp && cargo test test_parse_plot_md 2>&1
```

期待される出力（`parse_plot_md` がまだ存在しないため）：
```
error[E0425]: cannot find function `parse_plot_md` in this scope
```

- [ ] **Step 3: parse_plot_md を実装する**

`CharacterInfoTool` の定義より上、`#[cfg(test)]` ブロックより前に追加する。

```rust
/// plot.md の全文テキストを解析し、(章名, プロット本文) のリストを返す。
/// `# 章名` (level-1 見出し) を章の区切りとし、本文は trim 済みで返す。
/// level-2 以上の見出し・コード・その他コンテンツは本文として扱う。
pub(crate) fn parse_plot_md(content: &str) -> Vec<(String, String)> {
    let mut chapters: Vec<(String, String)> = Vec::new();
    let mut current_chapter: Option<String> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("# ") && !trimmed.starts_with("## ") {
            if let Some(name) = current_chapter.take() {
                chapters.push((name, current_body.trim().to_string()));
                current_body.clear();
            }
            current_chapter = Some(trimmed[2..].trim().to_string());
        } else if current_chapter.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if let Some(name) = current_chapter {
        chapters.push((name, current_body.trim().to_string()));
    }

    chapters
}
```

- [ ] **Step 4: テストがパスすることを確認する**

```
cd lsp && cargo test test_parse_plot_md 2>&1
```

期待される出力：
```
test tools::tests::test_parse_plot_md_basic ... ok
test tools::tests::test_parse_plot_md_level2_headings_treated_as_body ... ok
test tools::tests::test_parse_plot_md_trims_body ... ok
test tools::tests::test_parse_plot_md_empty_input ... ok
test tools::tests::test_parse_plot_md_content_before_first_heading_ignored ... ok

test result: ok. 5 passed; 0 failed
```

- [ ] **Step 5: コミットする**

```bash
git add lsp/src/tools.rs
git commit -m "feat(lsp): parse_plot_md実装"
```

---

### Task 3: PlotInfoTool を実装して completion() に追加する

**Files:**
- Modify: `lsp/src/tools.rs`
- Modify: `lsp/src/main.rs`

**Interfaces:**
- Consumes: `parse_plot_md(content: &str) -> Vec<(String, String)>` (Task 2)
- Produces: `pub(crate) fn PlotInfoTool::new(workspace: &Path) -> Box<dyn LlmTool>`
  - tool name: `"plot_info"`
  - 引数: `{ "chapter_name": string }` (省略可)
  - 返り値: 指定章のプロット本文、または全章を `# 章名\n本文` 形式で連結したテキスト

- [ ] **Step 1: PlotInfoTool を tools.rs に追加する**

`parse_plot_md` 関数の直後、`CharacterInfoTool` の前（または後）に追加する。

```rust
// ─── PlotInfoTool ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct PlotInfoTool {
    workspace: PathBuf,
}

impl PlotInfoTool {
    pub(crate) fn new(workspace: &Path) -> Box<dyn LlmTool> {
        Box::new(Self { workspace: workspace.to_path_buf() })
    }
}

#[async_trait]
impl LlmTool for PlotInfoTool {
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "chapter_name": {
                    "type": "string",
                    "description": "プロットを取得したい章の名前（省略時は全章を返す）"
                }
            },
            "required": []
        })
    }

    fn name(&self) -> &str { "plot_info" }
    fn description(&self) -> &str { "章のプロット情報を取得する" }

    async fn invoke(
        &self,
        args: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, LlmError> {
        let chapter_name = args.get("chapter_name").and_then(|v| v.as_str());

        let plot_path = self.workspace.join("plot.md");
        let content = tokio::fs::read_to_string(&plot_path)
            .await
            .map_err(|e| LlmError::GenericError {
                message: format!("plot.md を読み込めませんでした: {}", e),
            })?;

        let chapters = parse_plot_md(&content);

        match chapter_name {
            Some(name) => chapters
                .into_iter()
                .find(|(ch, _)| ch == name)
                .map(|(_, body)| body)
                .ok_or_else(|| LlmError::GenericError {
                    message: format!("章 '{}' が plot.md に見つかりません", name),
                }),
            None => {
                if chapters.is_empty() {
                    return Err(LlmError::GenericError {
                        message: "plot.md に章が見つかりません".to_string(),
                    });
                }
                Ok(chapters
                    .into_iter()
                    .map(|(name, body)| format!("# {}\n{}", name, body))
                    .collect::<Vec<_>>()
                    .join("\n\n"))
            }
        }
    }
}
```

- [ ] **Step 2: main.rs の completion() に PlotInfoTool を追加する**

`lsp/src/main.rs` の `completion()` 内、`tools::CharacterInfoTool::new(...)` の add_tool 呼び出しの直後に追加する。ワークスペースパスの取得は同じパターンで繰り返す。

```rust
// 既存（tools:: プレフィックスが付いた状態）
l.add_tool(tools::CharacterInfoTool::new(
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
// ↓ 追加
l.add_tool(tools::PlotInfoTool::new(
    &self
        .client
        .workspace_folders()
        .await
        .unwrap_or(None)
        .unwrap_or(vec![])
        .first()
        .map(|v| v.uri.to_file_path().unwrap())
        .unwrap_or_default(),
));
```

- [ ] **Step 3: ビルドと全テストを確認する**

```
cd lsp && cargo test 2>&1 | tail -20
```

期待される出力：
```
test result: ok. N passed; 0 failed; 0 ignored
```

- [ ] **Step 4: 動作確認**

ワークスペースルートに `plot.md` を作成する：
```markdown
# 第1章

主人公が港に到着する。嵐の予兆がある。

# 第2章

船が出航する。最初の試練。
```

LSP サーバを起動してエディタで `.txt` ファイルを編集し、補完を発火させる。debug ログで `plot_info` tool が呼ばれることを確認する：
```
[DEBUG] tool call plot_info({"chapter_name": "第1章"})
```

- [ ] **Step 5: コミットする**

```bash
git add lsp/src/tools.rs lsp/src/main.rs
git commit -m "feat(lsp): PlotInfoTool実装・completionに追加"
```
