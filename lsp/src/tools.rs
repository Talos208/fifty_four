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

// ─── PlotInfoTool (future) ───────────────────────────────────────────────────
// PlotInfoTool will be added here in a later task.
