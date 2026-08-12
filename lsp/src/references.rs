//! Find All References(`textDocument/references`)のための、LSP に依存しない純粋関数群。
//!
//! `main.rs` から切り出したモジュール。`Backend` から呼ばれる「ワークスペース走査」と
//! 「1ファイル分のテキストからの名前トークン抽出」を担う。キャラクター名の絞り込み自体は
//! `character::CharacterStore::lookup_names` が返す名前集合と
//! `Highlighter::is_recognized_name` に委譲し、ここでは持たない。

use crate::types::CachedLinderaToken;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tower_lsp_server::lsp_types::{Position, Range};
use tracing::instrument;

/// ワークスペース直下(非再帰)の `.txt` を列挙する(本文原稿のみ。
/// characters.md/plot.md/memo 等の設定・メモ類はスキャン対象にしない)。
#[cfg_attr(feature = "otel", tracing::instrument())]
pub(crate) fn discover_reference_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = root.read_dir() else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_txt = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("txt"))
            .unwrap_or(false);
        if path.is_file() && is_txt {
            files.push(path);
        }
    }
    files
}

/// `content` を行ごとに走査し、`names` のいずれかに一致する人名トークンの位置を全て返す。
///
/// `tokenize_line` はテストで差し替えられるよう引数で受け取る(本番は
/// `Highlighter::text_to_lindera_token`)。プレフィルタとして、`names` のどれも
/// 部分文字列として含まない行は形態素解析せずスキップする(トークンの表層形は必ず
/// 行の部分文字列なので、部分文字列すら無い行にヒットは有り得ず、結果は変わらない)。
#[instrument(skip(content, names, tokenize_line))]
pub(crate) fn scan_text(
    content: &str,
    names: &HashSet<String>,
    tokenize_line: &mut impl FnMut(&str) -> Vec<CachedLinderaToken>,
) -> Vec<Range> {
    let mut ranges = Vec::new();
    if names.is_empty() {
        return ranges;
    }

    for (line_no, line) in content.lines().enumerate() {
        if !names.iter().any(|n| line.contains(n.as_str())) {
            continue;
        }
        let tokens = tokenize_line(line);
        for tkn in &tokens {
            let surface = &line[tkn.byte_start..tkn.byte_end];
            if !crate::highlight::Highlighter::is_recognized_name(&tkn.details, surface, names) {
                continue;
            }
            let start_char = crate::types::utf16_len(&line[..tkn.byte_start]) as u32;
            let end_char = crate::types::utf16_len(&line[..tkn.byte_end]) as u32;
            let line_no = line_no as u32;
            ranges.push(Range::new(
                Position::new(line_no, start_char),
                Position::new(line_no, end_char),
            ));
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use indoc::indoc;

    /// テストごとに独立したディレクトリを用意する(既存テストと同じ temp_dir 方式)。
    fn fresh_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff_references_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_discover_reference_files_only_top_level_txt() {
        let root = fresh_root("discover");
        std::fs::write(root.join("chapter1.txt"), "").unwrap();
        std::fs::write(root.join("chapter2.TXT"), "").unwrap(); // 大文字拡張子も拾う
        std::fs::write(root.join("characters.md"), "").unwrap();
        std::fs::write(root.join("note.json"), "").unwrap();
        std::fs::create_dir_all(root.join("memo")).unwrap();
        std::fs::write(root.join("memo").join("nested.txt"), "").unwrap();

        let mut files: Vec<String> = discover_reference_files(&root)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(files, vec!["chapter1.txt", "chapter2.TXT"]);
    }

    #[test]
    fn test_discover_reference_files_missing_dir_returns_empty() {
        let root = std::env::temp_dir().join("ff_references_does_not_exist");
        let _ = std::fs::remove_dir_all(&root);
        assert!(discover_reference_files(&root).is_empty());
    }

    /// 実 `Highlighter` を使ってトークナイズするクロージャを組み立てる。
    fn tokenizer_for(names: &HashSet<String>) -> impl FnMut(&str) -> Vec<CachedLinderaToken> {
        let h = Highlighter::new();
        h.rebuild_user_dictionary(names).expect("辞書再構築に失敗");
        move |line: &str| h.text_to_lindera_token(line)
    }

    #[test]
    fn test_scan_text_finds_multiple_occurrences_with_utf16_columns() {
        let names = HashSet::from(["ジェフ・クライン".to_string()]);
        let mut tokenize = tokenizer_for(&names);
        let content = indoc!(
            "ジェフ・クラインは頷いた。
            艦橋は静かだった。
            もう一度、ジェフ・クラインが口を開く。
            "
        );
        let ranges = scan_text(content, &names, &mut tokenize);
        assert_eq!(ranges.len(), 2, "{:?}", ranges);

        assert_eq!(ranges[0].start, Position::new(0, 0));
        let expected_end0 = crate::types::utf16_len("ジェフ・クライン") as u32;
        assert_eq!(ranges[0].end, Position::new(0, expected_end0));

        assert_eq!(ranges[1].start.line, 2);
        let prefix = "もう一度、";
        let expected_start2 = crate::types::utf16_len(prefix) as u32;
        assert_eq!(ranges[1].start.character, expected_start2);

        // 日本語見出しはUTF-16長とバイト長が一致しない前提のテスト
        assert_ne!(expected_end0 as usize, "ジェフ・クライン".len());
    }

    #[test]
    fn test_scan_text_includes_alias_hits_in_same_result_set() {
        // 表示名+別名を同時に names へ渡すと、両方の登場箇所が1つの結果集合に混ざること
        let names = HashSet::from(["ジェフ・クライン".to_string(), "隊長".to_string()]);
        let mut tokenize = tokenizer_for(&names);
        let content = "ジェフ・クラインが敬礼した。隊長は頷いた。";
        let ranges = scan_text(content, &names, &mut tokenize);
        assert_eq!(ranges.len(), 2, "{:?}", ranges);
    }

    #[test]
    fn test_scan_text_does_not_match_unregistered_word() {
        let names = HashSet::from(["ジェフ・クライン".to_string()]);
        let mut tokenize = tokenizer_for(&names);
        let ranges = scan_text("シルビアが敬礼した。", &names, &mut tokenize);
        assert!(ranges.is_empty(), "{:?}", ranges);
    }

    #[test]
    fn test_scan_text_does_not_split_common_word_containing_registered_name() {
        // 1文字人名のユーザー辞書登録が、一般語("高原"等)の一部を誤ってヒットさせないこと
        // (highlight.rs の同種テストと同じ観点)。
        let names = HashSet::from(["原".to_string()]);
        let mut tokenize = tokenizer_for(&names);
        for word in ["高原", "原因", "原則"] {
            let ranges = scan_text(word, &names, &mut tokenize);
            assert!(ranges.is_empty(), "{}: {:?}", word, ranges);
        }
    }

    #[test]
    fn test_scan_text_prefilter_matches_naive_full_scan() {
        // プレフィルタ(行に部分文字列を含むかで足切り)を外した素朴な実装と
        // 結果が一致すること(取りこぼしが無いことの裏付け)。
        let names = HashSet::from(["ジェフ・クライン".to_string(), "隊長".to_string()]);
        let content = indoc!(
            "ジェフ・クラインは頷いた。
            艦橋は静かだった。
            隊長、前へ。
            無関係な行。
            "
        );

        let h = Highlighter::new();
        h.rebuild_user_dictionary(&names).expect("辞書再構築に失敗");

        let mut with_prefilter = |line: &str| h.text_to_lindera_token(line);
        let filtered = scan_text(content, &names, &mut with_prefilter);

        // プレフィルタ無しの素朴な実装: 全行を無条件にトークナイズする
        let mut naive = Vec::new();
        for (line_no, line) in content.lines().enumerate() {
            for tkn in h.text_to_lindera_token(line) {
                let surface = &line[tkn.byte_start..tkn.byte_end];
                if Highlighter::is_recognized_name(&tkn.details, surface, &names) {
                    let start_char = crate::types::utf16_len(&line[..tkn.byte_start]) as u32;
                    let end_char = crate::types::utf16_len(&line[..tkn.byte_end]) as u32;
                    naive.push(Range::new(
                        Position::new(line_no as u32, start_char),
                        Position::new(line_no as u32, end_char),
                    ));
                }
            }
        }

        assert_eq!(filtered, naive);
        assert!(!filtered.is_empty());
    }

    #[test]
    fn test_scan_text_empty_names_or_content_returns_empty() {
        let mut tokenize = |_: &str| Vec::new();
        assert!(scan_text("ジェフ・クライン", &HashSet::new(), &mut tokenize).is_empty());

        let names = HashSet::from(["ジェフ・クライン".to_string()]);
        let mut tokenize2 = tokenizer_for(&names);
        assert!(scan_text("", &names, &mut tokenize2).is_empty());
    }
}
