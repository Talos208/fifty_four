use crate::character::{CharacterAttribute, CharacterStore};
use crate::llm::{LlmError, LlmTool};
use crate::text::shorten_middle;
use async_trait::async_trait;
#[allow(unused_imports)]
use log::debug;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tracing::instrument;

// ─── parse_plot_md ───────────────────────────────────────────────────────────

/// plot.md の全文テキストを解析し、(章名, プロット本文) のリストを返す。
/// `# 章名` (level-1 見出し) を章の区切りとし、本文は trim 済みで返す。
/// level-2 以上の見出し・コード・その他コンテンツは本文として扱う。
/// front matter(あれば)は章本文に含めない。
///
/// 実体は `crate::plot::parse_plot` に委譲する(front matter の行オフセットや
/// メタ情報(episodes/average_chars)を必要とする inlay hint ハンドラと処理を共有するため)。
/// この関数は `PlotInfoTool` 向けの単純化されたビュー。
#[instrument]
pub(crate) fn parse_plot_md(content: &str) -> Vec<(String, String)> {
    crate::plot::parse_plot(content)
        .chapters
        .into_iter()
        .map(|c| (c.name, c.body))
        .collect()
}

// ─── CharacterInfoTool ───────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct CharacterInfoTool {
    workspace: PathBuf,
    store: CharacterStore,
}

#[async_trait]
impl LlmTool for CharacterInfoTool {
    #[instrument]
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

    #[instrument]
    fn name(&self) -> &str {
        "character_info"
    }

    #[instrument]
    fn description(&self) -> &str {
        "キャラクターの設定を取得する"
    }

    #[instrument(skip(_args))]
    async fn invoke(
        &self,
        _args: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, LlmError> {
        let name = _args["character_name"].as_str().unwrap_or("");
        let tags: Vec<CharacterAttribute> = _args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        v.as_str()
                            .and_then(|s| CharacterAttribute::try_from(s).ok())
                    })
                    .collect()
            })
            .unwrap_or_default();

        debug!("CharacterInfoTool({}, {:?})", name, tags);

        // character_store はワークスペース全ファイルをメモリに保持しているため、
        // 1ファイルへの決め打ちをせず全件横断で検索する
        // (呼称が複数ありうる問題は store 側の全件検索で解消済み)。
        let result = self.store.search(&self.workspace, name, &tags);
        debug!("\t{:?}", result.as_ref().map(|r| shorten_middle(r, 40)));
        result
    }
}

impl CharacterInfoTool {
    pub(crate) fn new(workspace: &Path, store: CharacterStore) -> Box<dyn LlmTool> {
        Box::new(Self {
            workspace: workspace.to_path_buf(),
            store,
        })
    }
}

// ─── PlotInfoTool ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct PlotInfoTool {
    workspace: PathBuf,
}

impl PlotInfoTool {
    pub(crate) fn new(workspace: &Path) -> Box<dyn LlmTool> {
        Box::new(Self {
            workspace: workspace.to_path_buf(),
        })
    }
}

#[async_trait]
impl LlmTool for PlotInfoTool {
    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
    fn name(&self) -> &str {
        "plot_info"
    }
    #[instrument(skip(self))]
    fn description(&self) -> &str {
        "章のプロット情報を取得する"
    }

    #[instrument(skip(self))]
    async fn invoke(
        &self,
        args: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, LlmError> {
        let chapter_name = args.get("chapter_name").and_then(|v| v.as_str());
        debug!("PlotInfoTool(chapter_name={:?})", chapter_name);

        let plot_path = self.workspace.join("plot.md");
        let content =
            tokio::fs::read_to_string(&plot_path)
                .await
                .map_err(|e| LlmError::GenericError {
                    message: format!("plot.md を読み込めませんでした: {}", e),
                })?;

        let chapters = parse_plot_md(&content);
        debug!(
            "PlotInfoTool: {} chapter(s) parsed from plot.md",
            chapters.len()
        );

        let result = match chapter_name {
            Some(name) => chapters
                .into_iter()
                .find(|(ch, _)| ch == name)
                .map(|(_, body)| body)
                .ok_or_else(|| LlmError::GenericError {
                    message: format!("章 '{}' が plot.md に見つかりません", name),
                }),
            None => {
                if chapters.is_empty() {
                    Err(LlmError::GenericError {
                        message: "plot.md に章が見つかりません".to_string(),
                    })
                } else {
                    Ok(chapters
                        .into_iter()
                        .map(|(name, body)| format!("# {}\n{}", name, body))
                        .collect::<Vec<_>>()
                        .join("\n\n"))
                }
            }
        };

        match &result {
            Ok(r) => debug!("PlotInfoTool: ok, {}", shorten_middle(r, 40)),
            Err(e) => debug!("PlotInfoTool: error, {}", e),
        }
        result
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
        assert!(
            chapters[0].1.contains("本文"),
            "level-2見出しは本文として扱う"
        );
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

    // front matter は「最初の見出し前」として無視されるため章本文に混入しない
    // (詳細な行番号・メタ情報の検証は crate::plot のテストで行う)。
    #[test]
    fn test_parse_plot_md_front_matter_excluded_from_body() {
        let md = "---\nepisodes: 3\naverage_chars: 4000\n---\n# 第1章\n内容。\n";
        let chapters = parse_plot_md(md);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].0, "第1章");
        assert_eq!(chapters[0].1, "内容。");
    }
}
