//! `plot.md` の解析(YAML front matter + `# 章名` 見出し区切り)。
//!
//! `tools.rs` の `PlotInfoTool` (LLM 向け) と `backend.rs` の inlay hint ハンドラ
//! (エディタ向け、章ごとの現文字数/予定文字数を表示する)の両方から使われる共通処理。
//! 章の区切りロジック自体はもともと `tools.rs::parse_plot_md` にあったものをここへ移し、
//! front matter 分の行オフセットを保持できるようにした。

use tracing::instrument;

/// front matter から取り出す作品全体のメタ情報。
///
/// フィールドが無い・型が合わない場合は `None`(欠如として扱い、呼び出し側が
/// 表示を省略する)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlotMeta {
    /// 話数(予定される章の総数)。
    pub episodes: Option<usize>,
    /// 1話あたりの平均(予定)文字数。
    pub average_chars: Option<usize>,
}

/// plot.md 中の1章分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlotChapter {
    pub name: String,
    pub body: String,
    /// `# 章名` 見出し行の、plot.md 原文における 0-based 行番号。
    pub heading_line: usize,
}

/// `parse_plot` の結果全体。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plot {
    pub meta: PlotMeta,
    pub chapters: Vec<PlotChapter>,
    /// front matter を閉じる `---` 行の 0-based 行番号(front matter が無ければ `None`)。
    pub front_matter_end_line: Option<usize>,
}

/// plot.md の全文テキストを解析する。
///
/// - 先頭行が `---` のときのみ front matter とみなし、次の `---` のみの行までを
///   メタ情報として切り離す(見出し走査からは除外する)。閉じられない場合は
///   front matter とはみなさない(誤検出よりは、見出し走査に巻き込む方が安全)。
/// - 章の区切りは `# 章名` (level-1 見出し)。level-2 以上の見出しやコード等は
///   本文として扱う。最初の見出しより前のテキスト(front matter を含む)は捨てる。
#[instrument]
pub(crate) fn parse_plot(content: &str) -> Plot {
    let front_matter_end_line = find_front_matter_end_line(content);
    let body_start_line = front_matter_end_line.map(|l| l + 1).unwrap_or(0);

    let (_, data) = crate::frontmatter::parse_text(content);
    let meta = PlotMeta {
        episodes: data.get("episodes").and_then(|s| s.trim().parse().ok()),
        average_chars: data
            .get("average_chars")
            .and_then(|s| s.trim().parse().ok()),
    };

    let mut chapters: Vec<PlotChapter> = Vec::new();
    let mut current: Option<(String, usize)> = None;
    let mut current_body = String::new();

    // `.enumerate()` を `.skip()` より前に置くことで、front matter 分を読み飛ばしても
    // 行番号 `i` は原文における絶対位置のまま保たれる。
    for (i, line) in content.lines().enumerate().skip(body_start_line) {
        let trimmed = line.trim_end();
        if trimmed.starts_with("# ") && !trimmed.starts_with("## ") {
            if let Some((name, heading_line)) = current.take() {
                chapters.push(PlotChapter {
                    name,
                    body: current_body.trim().to_string(),
                    heading_line,
                });
                current_body.clear();
            }
            current = Some((trimmed[2..].trim().to_string(), i));
        } else if current.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some((name, heading_line)) = current {
        chapters.push(PlotChapter {
            name,
            body: current_body.trim().to_string(),
            heading_line,
        });
    }

    Plot {
        meta,
        chapters,
        front_matter_end_line,
    }
}

/// 先頭行が `---` のときのみ、それを閉じる `---` のみの行の 0-based 行番号を返す。
/// 先頭行が `---` でない、または閉じる行が無ければ `None`。
#[instrument]
fn find_front_matter_end_line(content: &str) -> Option<usize> {
    let mut lines = content.lines().enumerate();
    let (_, first) = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    for (i, line) in lines {
        if line.trim_end() == "---" {
            return Some(i);
        }
    }
    None
}

/// 文字数を数える。改行(`\n`/`\r`)のみ除外し、空白・全角スペース等は数える。
pub(crate) fn count_chars(text: &str) -> usize {
    text.chars().filter(|c| *c != '\n' && *c != '\r').count()
}

/// カーソル位置までの文字数と1話あたりの予定文字数から、
/// LLM プロンプトへ埋め込む進捗説明文を返す(それ自体で完結した1文)。
///
/// `average_chars` が `0` の場合は呼び出し側で弾くべき条件(front matter 未設定など)
/// なので、ここでは呼ばれない前提とし空文字列を返す(ゼロ除算防止の最終防衛線)。
#[instrument]
pub(crate) fn progress_hint(chars_before_cursor: usize, average_chars: usize) -> String {
    if average_chars == 0 {
        return String::new();
    }
    let percent = chars_before_cursor * 100 / average_chars;
    if percent < 100 {
        format!("章のおよそ{percent}%あたりを執筆している。プロットを参照するとき参考にせよ。")
    } else {
        format!("章の終盤を執筆している。プロットを参照するとき参考にせよ。")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

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
    fn test_parse_plot_no_front_matter() {
        let plot = parse_plot(SAMPLE_PLOT);
        assert_eq!(plot.meta, PlotMeta::default());
        assert_eq!(plot.front_matter_end_line, None);
        assert_eq!(plot.chapters.len(), 3);
        assert_eq!(plot.chapters[0].name, "第1章");
        assert_eq!(plot.chapters[0].heading_line, 0);
        assert!(plot.chapters[0].body.contains("主人公が港に到着する"));
        assert_eq!(plot.chapters[1].name, "第2章");
        assert_eq!(plot.chapters[1].heading_line, 5);
        assert_eq!(plot.chapters[2].name, "第3章");
        assert_eq!(plot.chapters[2].heading_line, 9);
    }

    #[test]
    fn test_parse_plot_with_front_matter() {
        let md = indoc! {"
            ---
            episodes: 3
            average_chars: 4000
            ---
            # 第1章

            主人公が港に到着する。

            # 第2章

            船が出航する。
        "};
        let plot = parse_plot(md);
        assert_eq!(plot.meta.episodes, Some(3));
        assert_eq!(plot.meta.average_chars, Some(4000));
        assert_eq!(plot.front_matter_end_line, Some(3));
        assert_eq!(plot.chapters.len(), 2);
        // front matter 分(0-3行目)を挟んで4行目が見出し
        assert_eq!(plot.chapters[0].heading_line, 4);
        assert!(!plot.chapters[0].body.contains("episodes"));
        assert!(!plot.chapters[0].body.contains("---"));
        assert_eq!(plot.chapters[1].name, "第2章");
    }

    #[test]
    fn test_parse_plot_front_matter_missing_fields() {
        let md = indoc! {"
            ---
            episodes: 3
            ---
            # 第1章
            内容。
        "};
        let plot = parse_plot(md);
        assert_eq!(plot.meta.episodes, Some(3));
        assert_eq!(plot.meta.average_chars, None);
    }

    #[test]
    fn test_parse_plot_invalid_front_matter_yaml_falls_back() {
        // 不正なYAML(コロン無し)。frontmatter::parse_text はパース失敗として
        // 空のメタ情報にフォールバックするが、front matter ブロック自体の検出
        // (行スキャン)は YAML の妥当性に依存しないため章の区切りは崩れない。
        let md = indoc! {"
            ---
            this is not valid yaml: : :
            ---
            # 第1章
            内容。
        "};
        let plot = parse_plot(md);
        assert_eq!(plot.meta, PlotMeta::default());
        assert_eq!(plot.chapters.len(), 1);
        assert_eq!(plot.chapters[0].name, "第1章");
    }

    #[test]
    fn test_parse_plot_unterminated_front_matter_treated_as_no_front_matter() {
        // 閉じる `---` が無い場合、front matter とはみなさない。
        // 冒頭行(見出し前)としてそのまま無視される(既存の
        // 「最初の見出し前は無視」という挙動と同じ)。
        let md = "---\nepisodes: 3\n# 第1章\n内容。\n";
        let plot = parse_plot(md);
        assert_eq!(plot.front_matter_end_line, None);
        assert_eq!(plot.chapters.len(), 1);
        assert_eq!(plot.chapters[0].heading_line, 2);
    }

    #[test]
    fn test_parse_plot_level2_headings_treated_as_body() {
        let md = "# 第1章\n## サブセクション\n本文。\n# 第2章\n内容。\n";
        let plot = parse_plot(md);
        assert_eq!(plot.chapters.len(), 2);
        assert!(plot.chapters[0].body.contains("本文"));
    }

    #[test]
    fn test_parse_plot_empty_input() {
        assert!(parse_plot("").chapters.is_empty());
        assert!(parse_plot("見出しなしの本文のみ。").chapters.is_empty());
    }

    #[test]
    fn test_find_front_matter_end_line() {
        assert_eq!(find_front_matter_end_line("---\na: 1\n---\nbody"), Some(2));
        assert_eq!(find_front_matter_end_line("no front matter"), None);
        assert_eq!(find_front_matter_end_line("---\na: 1\n"), None);
        assert_eq!(find_front_matter_end_line(""), None);
    }

    #[test]
    fn test_count_chars_excludes_only_newlines() {
        assert_eq!(count_chars("あい\nう　え\r\n"), 5); // あ い う 　 え (全角スペース含む)
        assert_eq!(count_chars(""), 0);
        assert_eq!(count_chars("abc"), 3);
    }

    #[test]
    fn test_progress_hint_normal_case_includes_percent_and_chars() {
        let hint = progress_hint(1600, 4000);
        assert!(hint.contains("40%"));
        assert!(hint.contains("1600字"));
        assert!(hint.contains("4000字"));
        assert!(!hint.contains("超えて"));
    }

    #[test]
    fn test_progress_hint_chapter_start_is_zero_percent() {
        let hint = progress_hint(0, 4000);
        assert!(hint.contains("0%"));
        assert!(hint.contains("0字"));
    }

    #[test]
    fn test_progress_hint_exactly_at_average_is_over_wording() {
        // ちょうど予定文字数 = 100%相当は「超過」側の文言になる(percent < 100 の分岐)。
        let hint = progress_hint(4000, 4000);
        assert!(hint.contains("超えて"));
        assert!(hint.contains("終盤"));
    }

    #[test]
    fn test_progress_hint_over_average_uses_ending_wording() {
        let hint = progress_hint(4800, 4000);
        assert!(hint.contains("超えて"));
        assert!(hint.contains("終盤"));
        assert!(hint.contains("4800字"));
    }

    #[test]
    fn test_progress_hint_zero_average_returns_empty() {
        assert_eq!(progress_hint(100, 0), "");
    }
}
