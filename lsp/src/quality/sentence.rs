//! 行ベースの `LineData` を、品質診断ルールが扱いやすい「文」単位のビューへ組み直す。
//!
//! `LineData.tokens` の `meaning` は `Highlighter::ensure_line_state`(または
//! `tokenize_with_state`)による畳み込みが済んでいる前提(呼び出し元の `quality::analyze_document`
//! のドキュメントコメント参照)。ここでは新たにトークナイズし直さず、既にキャッシュされた
//! 情報だけを読む。

use std::ops::Range;

use crate::types::{LineData, TokenMeaning};

/// `CachedLinderaToken` を文書全体でフラットに並べ、行番号とバイト範囲を持たせた形に
/// コピーしたもの。ルール側は行をまたいだトークン列を単純な `&[STok]` として扱える。
#[derive(Debug, Clone)]
pub(crate) struct STok {
    /// 品詞情報。[0]=品詞, [1]=細分類1, [2]=細分類2, [3]=細分類3, [4]=活用型, [5]=活用形, [6]=原形
    /// (実測順。`CachedLinderaToken::details` のコメント参照)。
    pub details: [String; 7],
    pub meaning: TokenMeaning,
    /// 表層形(元テキストのスライス)。
    pub surface: String,
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
}

impl STok {
    pub(crate) fn pos(&self) -> &str {
        &self.details[0]
    }
    pub(crate) fn sub1(&self) -> &str {
        &self.details[1]
    }
    #[allow(dead_code)]
    pub(crate) fn sub2(&self) -> &str {
        &self.details[2]
    }
    /// 活用形(基本形/未然形/連用形 等)。実測上 `details[5]` に入っている
    /// (`CachedLinderaToken::details` のコメントは活用形/活用型の列順が実データと逆)。
    pub(crate) fn conj_form(&self) -> &str {
        &self.details[5]
    }
    pub(crate) fn base(&self) -> &str {
        &self.details[6]
    }
    pub(crate) fn is_proper_noun(&self) -> bool {
        self.pos() == "名詞" && self.sub1() == "固有名詞"
    }
    /// 括弧(台詞)の内側または括弧記号そのものかどうか。
    /// い抜き・ら抜き・二重敬語のような口語的な省略表現は台詞では正当な表現なので、
    /// これらのルールは台詞のトークンを除外する。
    pub(crate) fn in_dialogue(&self) -> bool {
        matches!(
            self.meaning,
            TokenMeaning::Bracket | TokenMeaning::InnerBracket | TokenMeaning::BracketClose
        )
    }
}

/// `lines` から文書順にフラットな `STok` 列を組み立てる。
#[allow(clippy::ptr_arg)]
pub(crate) fn build_tokens(lines: &[LineData]) -> Vec<STok> {
    let mut out = Vec::new();
    for (line_no, line) in lines.iter().enumerate() {
        for t in &line.tokens {
            let surface = line
                .text
                .get(t.byte_start..t.byte_end)
                .unwrap_or_default()
                .to_string();
            out.push(STok {
                details: t.details.clone(),
                meaning: t.meaning,
                surface,
                line: line_no,
                byte_start: t.byte_start,
                byte_end: t.byte_end,
            });
        }
    }
    out
}

/// フラットなトークン列における1文の範囲(`tokens` へのインデックス範囲)。
#[derive(Debug, Clone)]
pub(crate) struct SentenceSpan {
    pub range: Range<usize>,
    /// 文中に括弧(台詞)由来のトークンが1つでも含まれるか。
    /// 台詞は体言止め・口語表現が正当なため、文章作法系ルールの多くはこの文を除外する。
    pub in_dialogue: bool,
}

/// トークンが文の句点(文末)相当かどうか。
/// IPADIC は "。" を 記号,句点 として登録しているが、"！"/"？" は 記号,一般 になるため
/// 表層形での判定を併用する。
fn is_terminal_punct(t: &STok) -> bool {
    (t.pos() == "記号" && t.sub1() == "句点") || matches!(t.surface.as_str(), "!" | "?" | "！" | "？")
}

/// フラットなトークン列を文単位に分割する。
///
/// 境界は「句点相当のトークン」または「括弧閉じ(`TokenMeaning::BracketClose`)」。
/// 台詞は句点を伴わずに終わることが多いため、括弧閉じ単独でも文を区切る。
pub(crate) fn split_sentences(tokens: &[STok]) -> Vec<SentenceSpan> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut saw_dialogue = false;

    for (i, t) in tokens.iter().enumerate() {
        if matches!(
            t.meaning,
            TokenMeaning::Bracket | TokenMeaning::InnerBracket | TokenMeaning::BracketClose
        ) {
            saw_dialogue = true;
        }

        let is_boundary = is_terminal_punct(t) || t.meaning == TokenMeaning::BracketClose;
        if is_boundary {
            spans.push(SentenceSpan {
                range: start..i + 1,
                in_dialogue: saw_dialogue,
            });
            start = i + 1;
            saw_dialogue = false;
        }
    }

    if start < tokens.len() {
        spans.push(SentenceSpan {
            range: start..tokens.len(),
            in_dialogue: saw_dialogue,
        });
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{BracketColoring, Highlighter};
    use crate::types::TokenStatus;
    use std::collections::HashSet;
    use std::str::FromStr;

    fn fold(text: &str) -> Vec<LineData> {
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = text
            .lines()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        let mut state = TokenStatus::Normal;
        for line in &mut lines {
            let (_toks, s) =
                h.tokenize_with_state(line, state, BracketColoring::Distinct, &HashSet::new());
            state = s;
        }
        lines
    }

    #[test]
    fn test_split_sentences_by_period() {
        let lines = fold("彼は走った。彼は笑った。");
        let tokens = build_tokens(&lines);
        let spans = split_sentences(&tokens);
        assert_eq!(spans.len(), 2, "{:?}", spans);
        assert!(!spans[0].in_dialogue);
        assert!(!spans[1].in_dialogue);
    }

    #[test]
    fn test_split_sentences_dialogue_flagged() {
        let lines = fold("彼は「元気か」と聞いた。");
        let tokens = build_tokens(&lines);
        let spans = split_sentences(&tokens);
        // 「元気か」の括弧閉じでいったん区切れ、続く「と聞いた。」が2文目になる。
        assert_eq!(spans.len(), 2, "{:?}", spans);
        assert!(spans[0].in_dialogue, "{:?}", spans[0]);
        assert!(!spans[1].in_dialogue, "{:?}", spans[1]);
    }

    #[test]
    fn test_split_sentences_trailing_without_period() {
        let lines = fold("句点なしの断片");
        let tokens = build_tokens(&lines);
        let spans = split_sentences(&tokens);
        assert_eq!(spans.len(), 1, "{:?}", spans);
        assert_eq!(spans[0].range, 0..tokens.len());
    }

    #[test]
    fn test_split_sentences_across_lines() {
        // 行をまたいでも1つの文として扱われる(句点が最終行にしかない)。
        let lines = fold("これは\n複数行の文である。");
        let tokens = build_tokens(&lines);
        let spans = split_sentences(&tokens);
        assert_eq!(spans.len(), 1, "{:?}", spans);
        assert_eq!(tokens[spans[0].range.start].line, 0);
        assert_eq!(tokens[spans[0].range.end - 1].line, 1);
    }
}
