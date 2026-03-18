use crate::highlight::Highlighter;
use crate::types::{CachedLinderaToken, CursorContext, LineData, TokenStatus};
#[allow(unused_imports)]
use log::error;

fn is_bracket_open(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("括弧開")
}

fn is_bracket_close(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("括弧閉")
}

fn is_sentence_end(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("句点")
}

fn is_whitespace(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("空白")
}

fn before_token(
    texts: &[LineData],
    line_no: usize,
    token_index: usize,
    predicate: impl Fn(&CachedLinderaToken) -> bool,
    // ) -> Option<(&LineData, &CachedLinderaToken)> {
) -> Option<&CachedLinderaToken> {
    let mut tkn_ix = token_index as i64;
    let mut ln = line_no as i64;

    loop {
        let Some(line) = texts.get(ln as usize) else {
            return None;
        };

        tkn_ix -= 1;
        if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
            if predicate(tkn) {
                // return Some((line, tkn));
                return Some(tkn);
            }
        } else {
            ln -= 1;
            let Some(t) = texts.get(ln as usize) else {
                return None;
            };
            tkn_ix = t.tokens.len() as i64;
        }
    }
}

fn next_token(
    texts: &[LineData],
    line_no: usize,
    token_index: usize,
    predicate: impl Fn(&CachedLinderaToken) -> bool,
    // ) -> Option<(&LineData, &CachedLinderaToken)> {
) -> Option<&CachedLinderaToken> {
    let mut tkn_ix: i64 = token_index as i64;
    let mut ln = line_no;

    loop {
        let Some(line) = texts.get(ln) else {
            return None;
        };

        tkn_ix += 1;
        if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
            if predicate(tkn) {
                // return Some((line, tkn));
                return Some(tkn);
            }
        } else {
            ln += 1;
            tkn_ix = -1;
        }
    }
}

pub fn classify_complesion_mode(
    texts: &mut [LineData],
    line_no: usize,
    char_offset: usize,
    _hl: &Highlighter,
) -> CursorContext {
    // カーソル位置のトークンを取得
    let cursor_line = &texts[line_no];
    let cursor_line_text = &cursor_line.text;
    let byte_offset: usize = cursor_line_text
        .chars()
        .take(char_offset)
        .map(|c| c.len_utf8())
        .sum();

    let (cursor_ix, cursor_tkn) = cursor_line
        .tokens // Linderaは半角スペースをtokenにしない
        .iter()
        .enumerate()
        .find(|(_ix, tkn)| tkn.byte_start <= byte_offset && byte_offset < tkn.byte_end)
        .unwrap_or((cursor_line.tokens.len(), &CachedLinderaToken::EOT));

    let current_tkn = if cursor_tkn == CachedLinderaToken::EOT || is_whitespace(cursor_tkn) {
        next_token(texts, line_no, cursor_ix, |tkn| !is_whitespace(tkn))
    } else {
        Some(cursor_tkn)
    };

    // カーソル前の最後の非空白トークンを後方探索
    let before_tkn = before_token(texts, line_no, cursor_ix, |tkn| !is_whitespace(tkn));

    // tag ~~ とdepth ~~ を使って括弧の内外を判定
    // - tag == InBracket: 通常のトークンが括弧内にある
    // - is_bracket_open: 開き括弧の直後（自身はNormalだが直後は括弧内）
    // - depth > 0: ネストした括弧閉の直後でまだ外側の括弧内にいる場合
    let in_bracket = if let Some(tkn) = before_tkn {
        tkn.tag == TokenStatus::InBracket || is_bracket_open(tkn)
    } else {
        false
    };

    let last = before_tkn;
    let next = current_tkn;

    if !in_bracket {
        if last.is_some_and(|t| is_bracket_close(t)) {
            return CursorContext::AfterClosingBracket;
        }
        if last.is_some_and(|t| is_sentence_end(t)) {
            return CursorContext::AfterSentenceEnd;
        }
        return CursorContext::Other;
    }

    // 括弧内
    if last.is_some_and(|t| is_bracket_open(t)) && next.is_some_and(|t| is_bracket_close(t)) {
        return CursorContext::EmptyBracket;
    }
    if next.is_some_and(|t| is_bracket_close(t)) {
        return CursorContext::BeforeClosingBracket;
    }
    CursorContext::InBracketOther
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::str::FromStr;

    fn lines(text: &str) -> (Vec<LineData>, Highlighter) {
        let hl = Highlighter::new();
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        let mut line: Vec<LineData> = cr
            .split(text)
            .map(|l| LineData::from_str(l).unwrap())
            .collect();
        line.iter_mut().for_each(|l| {
            hl.tokenize(l);
        });
        (line, hl)
    }

    #[test]
    fn after_closing_bracket() {
        let (mut text, hl) = lines("彼は言った。「こんにちは」");
        let line = &text[0].text;
        let offset = line.chars().count(); // カーソルは末尾
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::AfterClosingBracket
        );
    }

    #[test]
    fn after_closing_bracket_with_whitespace() {
        let (mut text, hl) = lines("「こんにちは」\n  ");
        let offset = 2; // 空白の後
        assert_eq!(
            classify_complesion_mode(&mut text, 1, offset, &hl),
            CursorContext::AfterClosingBracket
        );
    }

    #[test]
    fn after_sentence_end() {
        let (mut text, hl) = lines("これは文章。");
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn after_sentence_end_with_newline() {
        let (mut text, hl) = lines("これは文章。\n");
        assert_eq!(
            classify_complesion_mode(&mut text, 1, 0, &hl),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn empty_bracket() {
        let (mut text, hl) = lines("「」");
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 1, &hl),
            CursorContext::EmptyBracket
        );
    }

    // TODO Linderaの挙動で、このパターンは適切に動かない
    /*
    #[test]
    fn empty_bracket_with_whitespace() {
        let (mut text, hl) = lines("「ほげ　」");
        let offset = "「ほげ".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::EmptyBracket
        );
    }
    */
    #[test]
    fn before_closing_bracket() {
        let (mut text, hl) = lines("「こんにちは」");
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn before_closing_bracket_multiline() {
        let (mut text, hl) = lines("「こんにちは\n」");
        // カーソルは1行目の末尾
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn in_bracket_other() {
        let (mut text, hl) = lines("「こんにちは、");
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::InBracketOther
        );
    }

    #[test]
    fn other_mid_sentence() {
        let (mut text, hl) = lines("これは途中");
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::Other
        );
    }

    #[test]
    fn other_empty_document() {
        let (mut text, hl) = lines("");
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, &hl),
            CursorContext::Other
        );
    }

    #[test]
    fn nested_brackets() {
        // 「『内側』|外側」 → depth=1なのでInBracketOther
        let (mut text, hl) = lines("「『内側』外側」");
        let offset = "「『内側』外側".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn after_nested_inner_close() {
        // 「『内側』|」 → depth=1, lastが括弧閉(内側の)だが外側はまだ開いている
        // → next_significantが」 → BeforeClosingBracket
        let (mut text, hl) = lines("「『内側』」");

        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, &hl),
            CursorContext::Other
        );

        let offset = "「『内側』".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, &hl),
            CursorContext::BeforeClosingBracket
        );
    }
}
