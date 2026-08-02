//! `character_updater` の適用フェーズ(`apply_plan`)向け、comrak AST ベースの
//! Markdownドキュメント編集プリミティブ。
//!
//! 旧実装は `replace_section`/`append_attribute_section` 等、行ベースの文字列
//! スプライシングで見出し検出・本文差し替えを独自に再実装していた(ATX見出しのみ対応、
//! コードフェンスの ``` のみ対応、見出しはレンダリング後テキストへの部分一致等)。
//!
//! ここでは代わりに、ただ1つのプリミティブに統一する:
//! **新規/置換テキストを対象ドキュメントと同じ `Arena` に追記パースし、
//! 結果のノード列を `insert_before`/`append`/`detach` で繋ぎ替える。**
//!
//! これにより `NodeValue::Heading` 等を手組みする必要が無くなり、ブロック境界の判定は
//! 常に comrak 自身のパーサに委譲される(フェンス誤検出・setext非対応等の脆さが構造的に無くなる)。
//!
//! `Arena` は `Send` ではないため、ここでの操作は `apply_plan` の `async` 関数内で
//! `.await` をまたがない同期スコープに閉じ込めて使うこと。

use crate::character::{CharacterAttribute, comrak_options, heading_text};
use comrak::Arena;
use comrak::nodes::{AstNode, NodeHeading, NodeValue};

/// 見出しノードのレベル(1〜6)を返す。見出しでなければ `None`。
fn heading_level(node: &AstNode) -> Option<u8> {
    match node.data.borrow().value {
        NodeValue::Heading(NodeHeading { level, .. }) => Some(level),
        _ => None,
    }
}

/// 見出しテキストを区切り文字（「・」「、」「,」「/」半角スペース）で分割し、
/// `CharacterAttribute` として解釈できるものだけを返す。
/// `replace_section`(旧実装)が使っていたのと同じ規則。
fn heading_attribute_tags(text: &str) -> Vec<CharacterAttribute> {
    text.split(['・', '、', ',', '/', ' '])
        .filter_map(|s| CharacterAttribute::try_from(s).ok())
        .collect()
}

/// `root` の直接の子から、見出しテキストが `char_name` を含む最初の見出しノードを探す。
/// 見つかった場合は (ノード, レベル) を返す。
fn find_char_heading<'a>(root: &'a AstNode<'a>, char_name: &str) -> Option<(&'a AstNode<'a>, u8)> {
    root.children().find_map(|n| {
        let level = heading_level(n)?;
        heading_text(n).contains(char_name).then_some((n, level))
    })
}

/// `start` の次の兄弟から走査し、レベルが `max_level` 以下の見出しに最初に出会ったノードを返す。
/// 見つからなければ `None`(＝ドキュメント末尾までがセクション)。
fn next_heading_at_or_above<'a>(start: &'a AstNode<'a>, max_level: u8) -> Option<&'a AstNode<'a>> {
    let mut cur = start.next_sibling();
    while let Some(n) = cur {
        if let Some(lv) = heading_level(n) {
            if lv <= max_level {
                return Some(n);
            }
        }
        cur = n.next_sibling();
    }
    None
}

/// `start`(exclusive)から `end`(exclusive、`None` ならドキュメント末尾まで)の間にある
/// 兄弟ノードを出現順に集める。
fn siblings_between<'a>(
    start: &'a AstNode<'a>,
    end: Option<&'a AstNode<'a>>,
) -> Vec<&'a AstNode<'a>> {
    let mut out = Vec::new();
    let mut cur = start.next_sibling();
    while let Some(n) = cur {
        if let Some(e) = end {
            if std::ptr::eq(n, e) {
                break;
            }
        }
        out.push(n);
        cur = n.next_sibling();
    }
    out
}

/// 1ファイル分のMarkdownドキュメントを保持し、複数の変異操作を逐次適用できるようにする。
/// 最終的に `render()` で1回だけCommonMark文字列化する。
pub struct FileDoc<'a> {
    arena: &'a Arena<'a>,
    root: &'a AstNode<'a>,
    options: comrak::Options<'static>,
}

impl<'a> FileDoc<'a> {
    /// 既存ファイルの内容(または新規ファイル用の空文字列)をパースする。
    pub fn parse(arena: &'a Arena<'a>, text: &str) -> Self {
        let mut options = comrak_options();
        // 行ベース時代の互換のためインデント型ではなくフェンス型でコードブロックを保持する。
        options.render.prefer_fenced = true;
        let root = comrak::parse_document(arena, text, &options);
        Self {
            arena,
            root,
            options,
        }
    }

    /// テキスト断片を、このドキュメントと同じ `arena` に追記パースする。
    /// 返り値の子ノード群は `self.root` へ `insert_before`/`append` でそのまま繋ぎ替えられる。
    fn parse_fragment(&self, text: &str) -> &'a AstNode<'a> {
        comrak::parse_document(self.arena, text, &self.options)
    }

    /// ドキュメント全体をCommonMarkとしてレンダリングする。
    pub fn render(&self) -> String {
        let mut buf = String::new();
        comrak::format_commonmark(self.root, &self.options, &mut buf)
            .expect("format_commonmark はメモリ上のバッファ書き込みで失敗しない");
        buf
    }

    /// 指定キャラクター・属性のセクション本文を丸ごと差し替える。
    /// 対象(キャラクター見出し、または属性見出し)が見つからない場合は `false`。
    /// `new_body` が空(trim後)の場合は本文を空にする(旧 `replace_section` と同じ)。
    pub fn replace_section(
        &self,
        char_name: &str,
        attribute: &CharacterAttribute,
        new_body: &str,
    ) -> bool {
        let Some((char_heading, char_level)) = find_char_heading(self.root, char_name) else {
            return false;
        };
        let char_block_end = next_heading_at_or_above(char_heading, char_level);

        // 属性見出し候補: キャラクターブロック内で、レベルが char_level+1 の見出し。
        let attr_heading = siblings_between(char_heading, char_block_end)
            .into_iter()
            .find(|n| {
                heading_level(n) == Some(char_level + 1)
                    && heading_attribute_tags(&heading_text(n)).contains(attribute)
            });
        let Some(attr_heading) = attr_heading else {
            return false;
        };

        // このセクションの終端: 属性見出し自身と同レベル以上の見出み、無ければキャラブロック終端。
        let section_end =
            next_heading_at_or_above(attr_heading, char_level + 1).or(char_block_end);

        for n in siblings_between(attr_heading, section_end) {
            n.detach();
        }

        if !new_body.trim().is_empty() {
            let fragment = self.parse_fragment(new_body);
            let new_nodes: Vec<_> = fragment.children().collect();
            for n in new_nodes {
                match section_end {
                    Some(end) => end.insert_before(n),
                    None => self.root.append(n),
                }
            }
        }

        true
    }

    /// 指定キャラクターの見出しブロック末尾に、新しい属性セクションを追記する。
    /// キャラクター見出しが見つからない場合は `false`。
    pub fn append_attribute_section(
        &self,
        char_name: &str,
        attr_heading: &str,
        body: &str,
    ) -> bool {
        let Some((char_heading, char_level)) = find_char_heading(self.root, char_name) else {
            return false;
        };
        let char_block_end = next_heading_at_or_above(char_heading, char_level);

        let attr_prefix = "#".repeat((char_level + 1) as usize);
        let fragment_text = format!("{} {}\n{}\n", attr_prefix, attr_heading, body.trim_end());
        let fragment = self.parse_fragment(&fragment_text);
        let new_nodes: Vec<_> = fragment.children().collect();
        for n in new_nodes {
            match char_block_end {
                Some(end) => end.insert_before(n),
                None => self.root.append(n),
            }
        }

        true
    }

    /// ドキュメント末尾に、新規キャラクターのブロックを全属性まとめて追記する。
    /// 見出しレベルは既存ドキュメントから推測し、不明な場合は `#`/`##` を使用する
    /// (`character::detect_char_level` に委譲)。
    pub fn append_new_character(&self, char_name: &str, sections: &[(&'static str, String)]) {
        let cl = crate::character::detect_char_level(self.root);
        let (cl, al) = if cl == 0 { (1, 2) } else { (cl, cl + 1) };
        let char_prefix = "#".repeat(cl as usize);
        let attr_prefix = "#".repeat(al as usize);

        let mut text = format!("{} {}\n", char_prefix, char_name);
        for (heading, body) in sections {
            text.push_str(&format!("\n{} {}\n{}\n", attr_prefix, heading, body.trim_end()));
        }

        let fragment = self.parse_fragment(&text);
        let new_nodes: Vec<_> = fragment.children().collect();
        for n in new_nodes {
            self.root.append(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, SAMPLE_MD);
        let ok = doc.replace_section(
            "ジェフ",
            &CharacterAttribute::Background,
            "- 予備役上がりで、元警備隊員。\n- フォン・ブラウン出身。",
        );
        assert!(ok);
        let out = doc.render();
        assert!(out.contains("予備役上がりで、元警備隊員"));
        assert!(out.contains("フォン・ブラウン出身"));
        assert!(
            out.contains("落ち着いている"),
            "他セクションが保持されること: {out}"
        );
        assert!(out.contains("シルビア"), "別キャラが保持されること: {out}");
    }

    #[test]
    fn test_replace_section_can_still_replace_body() {
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, SAMPLE_MD);
        let ok = doc.replace_section(
            "ジェフ",
            &CharacterAttribute::Background,
            "- 元警備隊員で予備役上がり。",
        );
        assert!(ok);
        let out = doc.render();
        assert!(out.contains("元警備隊員で予備役上がり"));
        assert!(
            !out.contains("- 予備役上がり。\n"),
            "replace_section 自体は指定本文へ差し替えること: {out}"
        );
        assert!(
            out.contains("落ち着いている"),
            "他セクションが保持されること: {out}"
        );
        assert!(out.contains("シルビア"), "別キャラが保持されること: {out}");
    }

    #[test]
    fn test_replace_section_not_found() {
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, SAMPLE_MD);
        // appearance セクションはないので false
        let ok = doc.replace_section("ジェフ", &CharacterAttribute::Appearance, "特になし");
        assert!(!ok);
    }

    #[test]
    fn test_replace_section_other_char_untouched() {
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, SAMPLE_MD);
        let ok = doc.replace_section(
            "シルビア",
            &CharacterAttribute::Personality,
            "- 真面目だが、緊張しやすい面もある。",
        );
        assert!(ok);
        let out = doc.render();
        assert!(
            out.contains("落ち着いている"),
            "ジェフのセクションが保持されること: {out}"
        );
        assert!(
            out.contains("真面目だが、緊張しやすい面もある"),
            "意味的に統合されたテキストを書けること: {out}"
        );
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
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, md);
        let ok = doc.append_attribute_section("ジェフ", "外見", "- 長身で白髪。");
        assert!(ok);
        let out = doc.render();
        // 新属性がジェフのブロック内（シルビアの前）に挿入される
        let jeff_end = out.find("## シルビア").unwrap_or(out.len());
        assert!(out.contains("外見"), "属性見出しがあること: {out}");
        assert!(
            out.find("外見").unwrap() < jeff_end,
            "シルビアより前に挿入されること: {out}"
        );
        assert!(
            out.contains("落ち着いている"),
            "既存属性が保持されること: {out}"
        );
        assert!(out.contains("シルビア"), "他キャラが保持されること: {out}");
    }

    #[test]
    fn test_append_attribute_section_at_eof() {
        // キャラが最後のブロック(次の同レベル見出しがない)
        let md = "\
## ジェフ
### 性格
- 落ち着いている。
";
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, md);
        let ok = doc.append_attribute_section("ジェフ", "外見", "- 長身。");
        assert!(ok);
        let out = doc.render();
        assert!(out.contains("外見"), "属性見出しがあること: {out}");
        assert!(out.contains("長身"), "属性本文があること: {out}");
        assert!(
            out.contains("落ち着いている"),
            "既存属性が保持されること: {out}"
        );
    }

    #[test]
    fn test_append_attribute_section_char_not_found() {
        let md = "## ジェフ\n### 性格\n- 落ち着いている。\n";
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, md);
        let ok = doc.append_attribute_section("存在しない", "外見", "- 不明。");
        assert!(!ok);
    }

    #[test]
    fn test_append_new_character() {
        let md = "\
# Story
## ジェフ
### 性格
- 落ち着いている。
";
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, md);
        doc.append_new_character("新キャラ", &[("背景", "- 謎の人物。".to_string())]);
        let out = doc.render();
        assert!(out.contains("新キャラ"), "新キャラ見出しがあること: {out}");
        assert!(out.contains("背景"), "属性見出しがあること: {out}");
        assert!(out.contains("謎の人物"), "属性本文があること: {out}");
        assert!(
            out.contains("落ち着いている"),
            "既存キャラが保持されること: {out}"
        );
        let jeff_pos = out.find("ジェフ").unwrap();
        let new_pos = out.find("新キャラ").unwrap();
        assert!(new_pos > jeff_pos, "新キャラが末尾に追加されること: {out}");
    }

    #[test]
    fn test_append_new_character_empty_file() {
        // 空ファイルはデフォルトレベル(1/2)を使用
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, "");
        doc.append_new_character("新キャラ", &[("性格", "- 明るい。".to_string())]);
        let out = doc.render();
        assert!(out.contains("# 新キャラ"), "レベル1のキャラ見出し: {out}");
        assert!(out.contains("## 性格"), "レベル2の属性見出し: {out}");
        assert!(out.contains("明るい"), "属性本文: {out}");
    }

    #[test]
    fn test_multiple_ops_applied_to_same_doc_in_sequence() {
        // apply_plan のファイル単位バッチ適用を模したシナリオ:
        // 同一ドキュメントへ「既存キャラの差し替え」「既存キャラへの追記」
        // 「新規キャラ追加」を順に適用しても、全て正しく反映されること。
        let arena = Arena::new();
        let doc = FileDoc::parse(&arena, SAMPLE_MD);

        assert!(doc.replace_section(
            "ジェフ",
            &CharacterAttribute::Background,
            "- 予備役上がりで、元警備隊員。",
        ));
        assert!(doc.append_attribute_section("ジェフ", "外見", "- 長身。"));
        doc.append_new_character("新キャラ", &[("性格", "- 明るい。".to_string())]);

        let out = doc.render();
        assert!(out.contains("元警備隊員"), "{out}");
        assert!(out.contains("外見"), "{out}");
        assert!(out.contains("長身"), "{out}");
        assert!(out.contains("新キャラ"), "{out}");
        assert!(out.contains("明るい"), "{out}");
        assert!(out.contains("シルビア"), "他キャラが保持されること: {out}");
        assert!(
            out.contains("落ち着いている"),
            "ジェフの他セクションが保持されること: {out}"
        );
    }
}
