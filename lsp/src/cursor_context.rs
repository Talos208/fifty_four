use crate::types::{CachedLinderaToken, CursorContext, LineData, TokenStatus};
#[allow(unused_imports)]
use log::{debug, error, trace};
use std::cmp::min;

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
    mut tokenize_line_no: impl FnMut(usize),
    predicate: impl Fn(&CachedLinderaToken) -> bool,
) -> Option<&CachedLinderaToken> {
    debug!("before_tkn");
    let mut tkn_ix = token_index as i64;
    let mut ln = line_no as i64;

    loop {
        // debug!("\t{},{}", ln, tkn_ix);
        let line = texts.get(ln as usize)?;
        if line.tokens.is_empty() {
            tokenize_line_no(ln as usize);
        }
        tkn_ix = min(tkn_ix, line.tokens.len() as i64);
        trace!("\t{},({} / {})", ln, tkn_ix, line.tokens.len() as i64);

        loop {
            tkn_ix -= 1;
            debug!("\t\t{}", tkn_ix);
            if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
                debug!("\t{}: {}-{}", tkn.details[6], tkn.byte_start, tkn.byte_end);
                if predicate(tkn) {
                    debug!("\tfound");
                    return Some(tkn);
                }
            } else {
                ln -= 1;
                tkn_ix = i64::MAX;
                break;
            }
        }
    }
}

fn next_token(
    texts: &[LineData],
    line_no: usize,
    token_index: usize,
    mut tokenize_line_no: impl FnMut(usize),
    predicate: impl Fn(&CachedLinderaToken) -> bool,
) -> Option<&CachedLinderaToken> {
    debug!("next_token");
    let mut tkn_ix: i64 = token_index as i64;
    let mut ln = line_no;

    loop {
        let line = texts.get(ln)?;
        if line.tokens.is_empty() {
            tokenize_line_no(ln);
        }

        loop {
            tkn_ix += 1;
            if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
                trace!("\t{}: {}-{}", ln, tkn.byte_start, tkn.byte_end);
                if predicate(tkn) {
                    debug!("\tfound");
                    return Some(tkn);
                }
            } else {
                ln += 1;
                tkn_ix = -1;
                break;
            }
        }
    }
}

pub fn classify_complesion_mode(
    texts: &mut [LineData],
    line_no: usize,
    char_offset: usize,
    mut tokenize_line_no: impl FnMut(usize),
) -> CursorContext {
    // カーソル位置のトークンを取得
    let cursor_line = &texts[line_no];
    if !cursor_line.text.is_empty() && cursor_line.tokens.is_empty() {
        tokenize_line_no(line_no);
    }
    let cursor_line_text = &cursor_line.text;

    let byte_offset: usize = cursor_line_text
        .chars()
        .take(char_offset)
        .map(|c| c.len_utf8())
        .sum();

    debug!(
        "line_no: {}, char_offset: {}, byte_offset: {}",
        line_no, char_offset, byte_offset
    );
    debug!("{}", cursor_line_text.chars().take(60).collect::<String>());

    let (cursor_ix, cursor_tkn) = cursor_line
        .tokens // Linderaは半角スペースをtokenにしない
        .iter()
        .enumerate()
        .find(|(_ix, tkn)| tkn.byte_start <= byte_offset && byte_offset < tkn.byte_end)
        .map(|(ix, tkn)| (ix, Some(tkn)))
        .unwrap_or_else(|| {
            debug!("Not found");
            let ix = cursor_line.tokens.len() - 1;
            if let Some(tkn) = next_token(texts, line_no, ix, &mut tokenize_line_no, |tkn| {
                !is_whitespace(tkn)
            }) {
                (ix, Some(tkn))
            } else {
                (cursor_line.tokens.len(), None)
            }
        });

    debug!(
        "current: {:?}",
        cursor_tkn.map(|t| (t.details[6].as_str(), t.details[0..=3].to_vec(), t.tag))
    );

    // カーソル前の最後の非空白トークンを後方探索
    let before_tkn = before_token(texts, line_no, cursor_ix, tokenize_line_no, |tkn| {
        !is_whitespace(tkn)
    });

    debug!(
        "before: {:?}",
        before_tkn.map(|t| (t.details[6].as_str(), t.details[0..=3].to_vec(), t.tag))
    );

    // tag ~~ とdepth ~~ を使って括弧の内外を判定
    // - tag == InBracket: 通常のトークンが括弧内にある
    // - is_bracket_open: 開き括弧の直後（自身はNormalだが直後は括弧内）
    // - depth > 0: ネストした括弧閉の直後でまだ外側の括弧内にいる場合
    let in_bracket = if let Some(tkn) = before_tkn {
        tkn.tag == TokenStatus::InBracket || is_bracket_open(tkn)
    } else {
        false
    };

    if !in_bracket {
        if before_tkn.is_some_and(is_bracket_close) {
            return CursorContext::AfterClosingBracket;
        }
        if before_tkn.is_some_and(is_sentence_end) {
            return CursorContext::AfterSentenceEnd;
        }
        return CursorContext::Other;
    }

    // 括弧内
    if before_tkn.is_some_and(is_bracket_open) && cursor_tkn.is_some_and(is_bracket_close) {
        return CursorContext::EmptyBracket;
    }
    if cursor_tkn.is_some_and(is_bracket_close) {
        return CursorContext::BeforeClosingBracket;
    }
    CursorContext::InBracketOther
}

#[cfg(test)]
mod tests {
    use crate::CursorContext;
    use crate::Highlighter;
    use crate::LineData;
    use crate::cursor_context::classify_complesion_mode;
    use regex::Regex;
    use std::str::FromStr;

    fn lines(text: &str) -> Vec<LineData> {
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        cr.split(text)
            .map(|l| LineData::from_str(l).unwrap())
            .collect()
    }

    struct TestData<'a> {
        texts: &'a mut Vec<LineData>,
        hl: Highlighter,
    }

    impl TestData<'_> {
        fn build<'a>(texts: &'a mut Vec<LineData>) -> Self {
            Self {
                texts,
                hl: Highlighter::new(),
            }
        }

        fn tokenize(&mut self, line_no: usize) {
            self.hl.tokenize(&mut self.texts[line_no]);
        }
    }

    fn tokenize(text: &str) -> (Vec<LineData>, Highlighter) {
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

    pub fn tokenize_line(hl: &Highlighter, texts: &mut Vec<LineData>, line_no: usize) {
        if !texts[line_no].tokens.is_empty() {
            return;
        }
        hl.tokenize(texts.get_mut(line_no).unwrap());
    }

    #[test]
    fn after_closing_bracket() {
        let mut texts = lines("彼は言った。「こんにちは」");
        let mut td = TestData::build(&mut texts);
        let line = &texts[0].text;
        let offset = line.chars().count(); // カーソルは末尾
        assert_eq!(
            classify_complesion_mode(&mut texts, 0, offset, |ln: usize| td.tokenize(ln)),
            CursorContext::AfterClosingBracket
        );
    }

    #[test]
    fn after_closing_bracket_with_whitespace() {
        let mut text = lines("思い出した。\n「」\n　故郷たる");
        let mut td = TestData::build(&mut text);
        assert_eq!(
            classify_complesion_mode(&mut text, 2, 1, |ln: usize| td.tokenize(ln)),
            CursorContext::AfterClosingBracket
        );
    }

    // #[test]
    // fn after_closing_bracket_with_whitespace() {
    //     let (mut text, hl) = lines("「こんにちは」\n  ");
    //     let offset = 2; // 空白の後
    //     assert_eq!(
    //         classify_complesion_mode(&mut text, 1, offset, &hl),
    //         CursorContext::AfterClosingBracket
    //     );
    // }

    #[test]
    fn after_sentence_end() {
        let mut text = lines("これは文章。");
        let mut td = TestData::build(&mut text);
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln: usize| td.tokenize(ln)),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn after_sentence_end_with_newline() {
        let mut text = lines("これは文章。\n");
        let mut td = TestData::build(&mut text);
        assert_eq!(
            classify_complesion_mode(&mut text, 1, 0, |ln: usize| td.tokenize(ln)),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn empty_bracket() {
        let mut text = lines("「」");
        let mut td = TestData::build(&mut text);
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 1, |ln: usize| td.tokenize(ln)),
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
        let mut text = lines("「こんにちは」");
        let mut td = TestData::build(&mut text);
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn before_closing_bracket_multiline() {
        let mut text = lines("「こんにちは\n」");
        let mut td = TestData::build(&mut text);
        // カーソルは1行目の末尾
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn in_bracket_other() {
        let mut text = lines("「こんにちは、");
        let mut td = TestData::build(&mut text);
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::InBracketOther
        );
    }

    #[test]
    fn other_mid_sentence() {
        let mut text = lines("これは途中");
        let mut td = TestData::build(&mut text);
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::Other
        );
    }

    #[test]
    fn other_empty_document() {
        let mut text = lines("");
        let mut td = TestData::build(&mut text);
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, |ln| td.tokenize(ln)),
            CursorContext::Other
        );
    }

    #[test]
    fn nested_brackets() {
        // 「『内側』|外側」 → depth=1なのでInBracketOther
        let mut text = lines("「『内側』外側」");
        let mut td = TestData::build(&mut text);
        let offset = "「『内側』外側".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn after_nested_inner_close() {
        // 「『内側』|」 → depth=1, lastが括弧閉(内側の)だが外側はまだ開いている
        // → next_significantが」 → BeforeClosingBracket
        let mut text = lines("「『内側』」");
        let mut td = TestData::build(&mut text);
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, |ln| td.tokenize(ln)),
            CursorContext::Other
        );

        let offset = "「『内側』".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |ln| td.tokenize(ln)),
            CursorContext::BeforeClosingBracket
        );
    }
}
