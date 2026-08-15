//! Markdown ドキュメントの見出し構造を LSP `textDocument/documentSymbol` 向けに抽出する。
//!
//! `character.rs` はキャラクター設定ファイル固有のドメインロジック(見出しレベル推定・
//! セクション分割)を担うのに対し、こちらは「見出しの階層構造を返す」だけの汎用処理。
//! `characters.md` / `plot.md` / `memo/*.md` のいずれにも同じ関数を使う。

use crate::types::utf16_len;
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, parse_document};
use tower_lsp_server::lsp_types::{DocumentSymbol, Position, Range, SymbolKind};
use tracing::instrument;

/// Markdown 全文から見出し(レベル, 名前, 0-based 行番号, 名前の UTF-16 終端 character)を
/// フラットな出現順のリストで返す。front matter・コードフェンス内の `#` はいずれも除外する
/// (`markdown_symbols` と `heading_line_levels` の共通処理)。
#[instrument]
fn collect_headings(content: &str) -> Vec<(u8, String, usize, usize)> {
    // `character::comrak_options()` をベースにしつつ、front matter の区切りだけ
    // 明示的に有効化する。plot.md の `---\nepisodes: 30\n---` を素の comrak に渡すと
    // 「水平線→パラグラフ→setext見出し」と誤読され、`episodes: 30` が偽の見出しとして
    // 混入するため(`plot.rs` が独自に front matter を切り離しているのと同じ理由)。
    // 共有の `comrak_options()` 自体は character.rs 側の挙動を変えないため上書きしない。
    let mut options = crate::character::comrak_options();
    options.extension.front_matter_delimiter = Some("---".to_string());

    let arena = Arena::new();
    let root = parse_document(&arena, content, &options);

    let mut headings: Vec<(u8, String, usize, usize)> = Vec::new();
    for node in root.children() {
        if let NodeValue::Heading(h) = node.data.borrow().value {
            let name = heading_inline_text(node);
            // sourcepos.start.line は1始まりなので0始まりへ変換する(character.rs と同じ流儀)。
            let line = node.data.borrow().sourcepos.start.line.saturating_sub(1);
            let end_character = utf16_len(&name);
            headings.push((h.level, name, line, end_character));
        }
    }
    headings
}

/// Markdown 全文から見出しの階層構造を `DocumentSymbol` の木として返す。
/// 見出しが1つも無ければ空 `Vec`。
#[instrument]
pub(crate) fn markdown_symbols(content: &str) -> Vec<DocumentSymbol> {
    let headings = collect_headings(content);
    let total_lines = content.lines().count();
    build_symbol_tree(&headings, total_lines)
}

/// Markdown 全文から見出し行を `(0-based 行番号, レベル)` のフラットなリストで返す。
/// `semantic_tokens_full` が見出し行の装飾(および他の装飾の抑制)対象を判定するために使う。
/// front matter・コードフェンス内の `#` は見出しとして扱わない(`markdown_symbols` と同じ判定)。
#[instrument]
pub(crate) fn heading_line_levels(content: &str) -> Vec<(usize, u8)> {
    collect_headings(content)
        .into_iter()
        .map(|(level, _, line, _)| (line, level))
        .collect()
}

/// 見出しノード配下のインラインテキストを走査して連結する。
///
/// `character::heading_text()` は見出しの直接の `Text` 子ノードのみを拾うため、
/// `# 第一章 **決戦**` のような強調・リンクを含む見出しでは装飾部分が抜け落ちる
/// (`**決戦**` は `Text` の子ではなく `Strong` の孫)。ここでは `Text`/`Code` を
/// 深さ問わず拾い、`Emph`/`Strong`/`Link` 等はコンテナとして単に降りる。
#[instrument]
fn heading_inline_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut result = String::new();
    for edge in node.traverse() {
        if let NodeEdge::Start(n) = edge {
            match &n.data.borrow().value {
                NodeValue::Text(cow) => result.push_str(cow.as_ref()),
                NodeValue::Code(code) => result.push_str(&code.literal),
                _ => {}
            }
        }
    }
    result
}

/// フラットな見出しリストから `DocumentSymbol` の木を構築する。
///
/// スタックに (level, 構築中の DocumentSymbol) を積み、新しい見出しの level 以上を
/// スタック最上位から pop → 親の children へ収める、を繰り返す定番アルゴリズム。
/// レベル飛び(`#` の直後に `###`)でもスタックが単調減少にしかならないため、
/// 特別扱い無しで「###がすぐ#の子になる」という自然な結果になる。
fn build_symbol_tree(
    headings: &[(u8, String, usize, usize)],
    total_lines: usize,
) -> Vec<DocumentSymbol> {
    // 各見出しの range 終端行を先に決める:
    // 「自分より後ろで、自分の level 以下の最初の見出し」の直前行、無ければ文書末行。
    let mut stack: Vec<(u8, DocumentSymbol)> = Vec::new();
    let mut roots: Vec<DocumentSymbol> = Vec::new();

    for (ix, &(level, ref name, line, end_character)) in headings.iter().enumerate() {
        let end_line = headings[ix + 1..]
            .iter()
            .find(|&&(l, _, _, _)| l <= level)
            .map(|&(_, _, next_line, _)| next_line.saturating_sub(1))
            .unwrap_or(total_lines.saturating_sub(1));

        let selection_range = Range {
            start: Position {
                line: line as u32,
                character: 0,
            },
            end: Position {
                line: line as u32,
                character: end_character as u32,
            },
        };
        let range = Range {
            start: selection_range.start,
            end: Position {
                line: end_line.max(line) as u32,
                character: 0,
            },
        };

        #[allow(deprecated)]
        let symbol = DocumentSymbol {
            name: name.clone(),
            detail: None,
            kind: SymbolKind::STRING,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: None,
        };

        // スタック最上位が自分と同レベル以下(=親になれない)の間、閉じて親へ収める。
        while let Some(&(top_level, _)) = stack.last() {
            if top_level >= level {
                let (_, finished) = stack.pop().unwrap();
                match stack.last_mut() {
                    Some((_, parent)) => {
                        parent.children.get_or_insert_with(Vec::new).push(finished);
                    }
                    None => roots.push(finished),
                }
            } else {
                break;
            }
        }
        stack.push((level, symbol));
    }

    while let Some((_, finished)) = stack.pop() {
        match stack.last_mut() {
            Some((_, parent)) => {
                parent.children.get_or_insert_with(Vec::new).push(finished);
            }
            None => roots.push(finished),
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    /// `text` 内で `needle` を含む行の 0-based 行番号を返す。
    fn line_of(text: &str, needle: &str) -> usize {
        text.lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("line containing {needle:?} not found"))
    }

    #[test]
    fn test_markdown_symbols_empty_document_returns_empty() {
        assert!(markdown_symbols("").is_empty());
        assert!(markdown_symbols("見出しの無い本文だけ。\n").is_empty());
    }

    #[test]
    fn test_markdown_symbols_nested_headings_build_parent_child() {
        let md = indoc! {"
            # 章
            ## 節
            ### 項
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "章");
        let children = symbols[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "節");
        let grandchildren = children[0].children.as_ref().unwrap();
        assert_eq!(grandchildren.len(), 1);
        assert_eq!(grandchildren[0].name, "項");
    }

    #[test]
    fn test_markdown_symbols_level_skip_becomes_child() {
        let md = indoc! {"
            # 章
            ### 項
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        let children = symbols[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "項");
    }

    #[test]
    fn test_markdown_symbols_siblings_stay_at_same_depth() {
        let md = indoc! {"
            ## 節A
            ## 節B
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "節A");
        assert_eq!(symbols[1].name, "節B");
    }

    #[test]
    fn test_markdown_symbols_ignores_hash_inside_fenced_code_block() {
        let md = indoc! {"
            # 章
            ```
            # これはコードブロック内なので見出しではない
            ```
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "章");
        assert!(symbols[0].children.is_none());
    }

    #[test]
    fn test_markdown_symbols_picks_up_setext_heading() {
        let md = indoc! {"
            見出し
            ======
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "見出し");
    }

    #[test]
    fn test_markdown_symbols_front_matter_is_not_treated_as_heading() {
        let md = indoc! {"
            ---
            episodes: 30
            average_chars: 3000
            ---
            # 第一章
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "第一章");
    }

    #[test]
    fn test_markdown_symbols_decorated_heading_keeps_full_text() {
        let md = "# 第一章 **決戦**\n";
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "第一章 決戦");
    }

    #[test]
    fn test_markdown_symbols_range_ends_before_next_same_level_heading() {
        let md = indoc! {"
            # 章A
            本文A
            # 章B
            本文B
        "};
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 2);

        let heading_a = line_of(md, "章A") as u32;
        let heading_b = line_of(md, "章B") as u32;

        assert_eq!(symbols[0].selection_range.start.line, heading_a);
        assert_eq!(symbols[0].selection_range.end.line, heading_a);
        // 章Aのrangeは章Bの直前行(本文A)で終わる。
        assert_eq!(symbols[0].range.end.line, heading_b - 1);

        // selection_range は range に包含される。
        assert!(symbols[0].range.start.line <= symbols[0].selection_range.start.line);
        assert!(symbols[0].range.end.line >= symbols[0].selection_range.end.line);
    }

    #[test]
    fn test_markdown_symbols_japanese_heading_uses_utf16_length_not_byte_length() {
        let md = "# 日本語見出し\n";
        let symbols = markdown_symbols(md);
        assert_eq!(symbols.len(), 1);
        let name = &symbols[0].name;
        let utf16_length = utf16_len(name) as u32;
        let byte_length = name.len() as u32;
        // マルチバイト文字なのでバイト長とUTF-16長は一致しない。
        assert_ne!(utf16_length, byte_length);
        assert_eq!(symbols[0].selection_range.end.character, utf16_length);
    }

    // ---- heading_line_levels ----

    #[test]
    fn test_heading_line_levels_returns_flat_line_level_pairs() {
        let md = indoc! {"
            # 章
            本文
            ## 節
        "};
        assert_eq!(heading_line_levels(md), vec![(0, 1), (2, 2)]);
    }

    #[test]
    fn test_heading_line_levels_excludes_front_matter_and_code_fence() {
        let md = indoc! {"
            ---
            episodes: 30
            ---
            # 第一章
            ```
            # コードブロック内
            ```
        "};
        assert_eq!(heading_line_levels(md), vec![(3, 1)]);
    }

    #[test]
    fn test_heading_line_levels_empty_document_returns_empty() {
        assert!(heading_line_levels("").is_empty());
        assert!(heading_line_levels("見出しの無い本文だけ。\n").is_empty());
    }
}
