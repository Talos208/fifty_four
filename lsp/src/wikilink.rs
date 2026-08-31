//! `[[ページ名]]` wikilink の解析・解決・展開。`references.rs`/`character.rs` と同じ方針で
//! LSP 型に依存しない純粋関数群にしてある。記法・解決規則の詳細は `docs/lsp-handlers.md`
//! の「wikilink」参照。

use comrak::nodes::NodeValue;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::instrument;

/// 1件の wikilink の検出結果。位置は LSP の `Position` にそのまま使える形
/// (0-based 行番号、UTF-16 コード単位の列)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WikilinkMatch {
    /// `[[target]]`/`[[表示名|target]]` の `target` 部分(pipe の前後は `docs/lsp-handlers.md`
    /// 参照。Obsidian と逆順なので注意)。
    pub(crate) target: String,
    /// 0-based 行番号。
    pub(crate) line: u32,
    /// 開始位置の UTF-16 コード単位オフセット(行内)。
    pub(crate) start_utf16: u32,
    /// 終了位置の UTF-16 コード単位オフセット(行内)。
    pub(crate) end_utf16: u32,
}

/// `content` から `[[target]]`/`[[表示名|target]]` 記法の wikilink を全て検出する。
/// 位置は 0-based 行番号 + UTF-16 オフセットへ変換済み(comrak は 1-based 行 + UTF-8
/// バイトオフセットで返すため。詳細は `docs/lsp-handlers.md` の「位置(UTF-16)の扱い」参照)。
#[instrument(ret)]
pub(crate) fn find_wikilinks(content: &str) -> Vec<WikilinkMatch> {
    let lines: Vec<&str> = content.lines().collect();
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, content, &crate::character::comrak_options());

    let mut out = Vec::new();
    for node in root.descendants() {
        let NodeValue::WikiLink(wl) = &node.data().value else {
            continue;
        };
        let pos = node.data().sourcepos;
        // comrak の line/column は 1-based。`end.column` は末尾 `]` の列(inclusive)なので、
        // 排他的な終端バイトオフセットはそのまま `end.column` になる
        // (comrak 側は `make_inline(.., startpos - 1, scanner.pos - 1)` の両引数に +1 する)。
        let Some(start_line_text) = lines.get(pos.start.line.saturating_sub(1)) else {
            continue;
        };
        let Some(end_line_text) = lines.get(pos.end.line.saturating_sub(1)) else {
            continue;
        };
        let start_byte = pos.start.column.saturating_sub(1);
        let end_byte = pos.end.column;
        // 想定外の sourcepos(行外・多バイト文字の途中)は panic させず無視する。
        let (Some(before_start), Some(before_end)) = (
            start_line_text.get(..start_byte),
            end_line_text.get(..end_byte),
        ) else {
            continue;
        };
        out.push(WikilinkMatch {
            target: wl.url.clone(),
            line: (pos.start.line - 1) as u32,
            start_utf16: crate::types::utf16_len(before_start) as u32,
            end_utf16: crate::types::utf16_len(before_end) as u32,
        });
    }
    out
}

/// wikilink のターゲット文字列を `current_dir` 相対でファイルパスへ解決する(拡張子省略時は
/// `.md` を補う)。実在しなければ `None`(自動作成しない。解決規則の詳細は
/// `docs/lsp-handlers.md` 参照)。
#[instrument(ret)]
pub(crate) fn resolve_target(current_dir: &Path, target: &str) -> Option<PathBuf> {
    let path = PathBuf::from(target);
    let path = if path.extension().is_some() {
        path
    } else {
        path.with_extension("md")
    };
    let resolved = current_dir.join(path);
    resolved.is_file().then_some(resolved)
}

/// `start` 自身を含め、本文中の wikilink を推移的にたどって到達可能な全ファイルを
/// `(パス, そのファイル自身の内容)` の列で返す(`character_updater` の更新先候補列挙用)。
/// `visited`(正規化パス)で循環参照を防ぐ。読み込みに失敗したリンク先はスキップして続行する。
#[instrument(skip(start_content), ret)]
pub(crate) fn expand_files(start: &Path, start_content: &str) -> Vec<(PathBuf, String)> {
    let mut visited = HashSet::new();
    if let Ok(canon) = start.canonicalize() {
        visited.insert(canon);
    }
    let mut out = vec![(start.to_path_buf(), start_content.to_string())];
    collect_linked_files(start_content, start, &mut visited, &mut out);
    out
}

fn collect_linked_files(
    content: &str,
    current: &Path,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<(PathBuf, String)>,
) {
    let Some(current_dir) = current.parent() else {
        return;
    };
    for m in find_wikilinks(content) {
        let Some(target_path) = resolve_target(current_dir, &m.target) else {
            continue;
        };
        let canon = target_path.canonicalize().unwrap_or_else(|_| target_path.clone());
        if !visited.insert(canon) {
            // 既訪問(循環参照、または同じページへの複数リンク): 展開しない。
            continue;
        }
        let Ok(linked_content) = std::fs::read_to_string(&target_path) else {
            continue;
        };
        out.push((target_path.clone(), linked_content.clone()));
        collect_linked_files(&linked_content, &target_path, visited, out);
    }
}

/// `start` の内容を読み、本文中の wikilink を推移的にたどって境界なく連結する。
/// `expand_files` の内容部分を連結しただけの薄いラッパ(現状の呼び出し元は無いが、
/// ファイル単位の区別が不要な用途向けに残してある)。
#[instrument(skip(start_content), ret)]
#[allow(dead_code)]
pub(crate) fn expand_content(start: &Path, start_content: &str) -> String {
    expand_files(start, start_content)
        .into_iter()
        .map(|(_, c)| c)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- find_wikilinks ----

    #[test]
    fn test_find_wikilinks_simple() {
        let out = find_wikilinks("前置き[[ページA]]後書き");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, "ページA");
        assert_eq!(out[0].line, 0);
    }

    #[test]
    fn test_find_wikilinks_pipe_syntax_target_after_pipe() {
        // TitleFirst設定なので表示名が先(docs/lsp-handlers.md参照)。
        let out = find_wikilinks("[[表示名|ページA]]");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, "ページA");
    }

    #[test]
    fn test_find_wikilinks_multiple_per_line() {
        let out = find_wikilinks("[[A]]と[[B]]は関係がある");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].target, "A");
        assert_eq!(out[1].target, "B");
    }

    #[test]
    fn test_find_wikilinks_line_number_is_zero_based() {
        let out = find_wikilinks("1行目\n2行目の[[ページA]]");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 1, "2行目(0-basedなのでindex 1)にあること");
    }

    #[test]
    fn test_find_wikilinks_utf16_offset_correct_when_japanese_precedes_link() {
        // UTF-8バイトオフセットのままだとズレる回帰テスト("田中太郎です。"=7文字)。
        let out = find_wikilinks("田中太郎です。[[ページA]]");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_utf16, 7);
    }

    #[test]
    fn test_find_wikilinks_range_covers_whole_link_including_closing_brackets() {
        // comrak の end.column は末尾 `]` の列(inclusive)。-1 すると `]` を取りこぼし、
        // DocumentLink の範囲が1文字短くなる回帰テスト。
        let text = "[[ページA]]";
        let out = find_wikilinks(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_utf16, 0);
        assert_eq!(
            out[0].end_utf16,
            crate::types::utf16_len(text) as u32,
            "`[[` から `]]` までの全体を覆うこと"
        );
    }

    #[test]
    fn test_find_wikilinks_range_utf16_when_japanese_precedes_link() {
        let text = "田中太郎です。[[ページA]]";
        let out = find_wikilinks(text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_utf16, 7);
        assert_eq!(out[0].end_utf16, crate::types::utf16_len(text) as u32);
    }

    // ---- resolve_target ----

    fn make_tree(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff_wikilink_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
        }
        dir
    }

    #[test]
    fn test_resolve_target_infers_md_extension() {
        let dir = make_tree("resolve_infer_ext", &["ページA.md"]);
        let resolved = resolve_target(&dir, "ページA");
        assert_eq!(resolved, Some(dir.join("ページA.md")));
    }

    #[test]
    fn test_resolve_target_missing_returns_none() {
        let dir = make_tree("resolve_missing", &[]);
        let resolved = resolve_target(&dir, "存在しない");
        assert_eq!(resolved, None);
    }

    #[test]
    fn test_resolve_target_does_not_fallback_outside_current_dir() {
        // current_dir の外(親)にファイルがあっても解決しない(ワークスペース全体探索はしない)。
        let dir = make_tree("resolve_no_fallback", &["ページA.md"]);
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let resolved = resolve_target(&sub, "ページA");
        assert_eq!(resolved, None);
    }

    // ---- expand_content ----

    #[test]
    fn test_expand_content_transitive_a_b_c() {
        let dir = make_tree("expand_transitive", &["A.md", "B.md", "C.md"]);
        std::fs::write(dir.join("A.md"), "本文A[[B]]").unwrap();
        std::fs::write(dir.join("B.md"), "本文B[[C]]").unwrap();
        std::fs::write(dir.join("C.md"), "本文C").unwrap();
        let expanded = expand_content(&dir.join("A.md"), "本文A[[B]]");
        assert!(expanded.contains("本文A"));
        assert!(expanded.contains("本文B"));
        assert!(expanded.contains("本文C"));
    }

    #[test]
    fn test_expand_content_cycle_does_not_loop_forever() {
        let dir = make_tree("expand_cycle", &["A.md", "B.md"]);
        std::fs::write(dir.join("A.md"), "本文A[[B]]").unwrap();
        std::fs::write(dir.join("B.md"), "本文B[[A]]").unwrap();
        // 停止すること自体がテスト(無限ループなら test がハングする)。
        let expanded = expand_content(&dir.join("A.md"), "本文A[[B]]");
        assert!(expanded.contains("本文A"));
        assert!(expanded.contains("本文B"));
    }

    #[test]
    fn test_expand_content_skips_missing_link_and_continues() {
        let dir = make_tree("expand_missing_link", &["A.md"]);
        std::fs::write(dir.join("A.md"), "本文A[[存在しない]]").unwrap();
        let expanded = expand_content(&dir.join("A.md"), "本文A[[存在しない]]");
        assert_eq!(expanded, "本文A[[存在しない]]");
    }

    // ---- expand_files ----

    #[test]
    fn test_expand_files_matches_expand_content_concatenation() {
        let dir = make_tree("expand_files_matches", &["A.md", "B.md", "C.md"]);
        std::fs::write(dir.join("A.md"), "本文A[[B]]").unwrap();
        std::fs::write(dir.join("B.md"), "本文B[[C]]").unwrap();
        std::fs::write(dir.join("C.md"), "本文C").unwrap();
        let files = expand_files(&dir.join("A.md"), "本文A[[B]]");
        let concatenated: String = files.iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(concatenated, expand_content(&dir.join("A.md"), "本文A[[B]]"));
    }

    #[test]
    fn test_expand_files_includes_start_and_target_paths() {
        let dir = make_tree("expand_files_paths", &["A.md", "B.md"]);
        std::fs::write(dir.join("A.md"), "本文A[[B]]").unwrap();
        std::fs::write(dir.join("B.md"), "本文B").unwrap();
        let files = expand_files(&dir.join("A.md"), "本文A[[B]]");
        let paths: Vec<&PathBuf> = files.iter().map(|(p, _)| p).collect();
        assert!(paths.contains(&&dir.join("A.md")));
        assert!(paths.contains(&&dir.join("B.md")));
    }
}
