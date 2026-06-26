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

// ─── parse_plot_md ───────────────────────────────────────────────────────────

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
        // {
        //     let cache = self.cache.0.lock();
        //     if let Some(entry) = cache.get(&path)
        //         && entry.modified == modified
        //     {
        //         let result = Self::search_cache(entry, name, &tags);
        //         debug!("\t{:?}", result.map(|r| shorten_middle(&r, 40)))
        //     }
        // }

        // キャッシュミス: ファイルを読んでパース
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
        Box::new(Self {
            workspace: workspace.to_path_buf(),
            cache,
        })
    }

    fn find_character_file_path(&self, name: &str) -> std::result::Result<PathBuf, LlmError> {
        find_character_file_path(&self.workspace, name)
    }

    pub(crate) fn search_cache(
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
