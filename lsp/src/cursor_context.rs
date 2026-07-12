use crate::types::{CachedLinderaToken, CursorContext, LineData, TokenStatus};
use dashmap::DashMap;
#[allow(unused_imports)]
use log::{debug, error, trace};
use std::cmp::max;
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

fn before_token_inline(
    line: &LineData,
    token_index: usize,
    predicate: impl Fn(CachedLinderaToken) -> bool,
) -> (usize, Option<CachedLinderaToken>) {
    let mut tkn_ix = token_index as i64;
    loop {
        tkn_ix -= 1;
        if tkn_ix < 0 {
            break;
        }
        // debug!("\t\t{}", tkn_ix);
        if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
            trace!("\t{}: {}-{}", tkn.details[6], tkn.byte_start, tkn.byte_end);
            if predicate(tkn.clone()) {
                trace!("\tfound");
                return (tkn_ix as usize, Some(tkn.clone()));
            }
        } else {
            break;
        }
    }

    (0, None)
}

fn before_token(
    texts: &mut [LineData],
    line_no: usize,
    token_index: usize,
    mut tokenize_line_no: impl FnMut(&mut LineData),
    predicate: impl Fn(CachedLinderaToken) -> bool,
) -> (usize, usize, Option<CachedLinderaToken>) {
    debug!("before_tkn");
    let mut tkn_ix = token_index as i64;
    let mut ln = line_no as i64;

    loop {
        if texts.get(ln as usize).is_none() {
            return (max(0, ln) as usize, 0, None);
        }
        if texts[ln as usize].tokens.is_empty() {
            tokenize_line_no(&mut texts[ln as usize]);
        }
        let line = &texts[ln as usize];
        tkn_ix = min(tkn_ix, line.tokens.len() as i64);
        trace!("\t{},({} / {})", ln, tkn_ix, line.tokens.len() as i64);

        let (tix, tkn) = before_token_inline(line, tkn_ix as usize, &predicate);
        match tkn {
            Some(tkn) => return (ln as usize, tix, Some(tkn)),
            None => {
                ln -= 1;
                tkn_ix = i64::MAX;
            }
        }
    }
}

fn next_token(
    texts: &mut [LineData],
    line_no: usize,
    token_index: usize,
    mut tokenize_line_no: impl FnMut(&mut LineData),
    predicate: impl Fn(CachedLinderaToken) -> bool,
) -> Option<CachedLinderaToken> {
    debug!("next_token");
    let mut tkn_ix: i64 = token_index as i64;
    let mut ln = line_no;

    loop {
        texts.get(ln)?;
        if texts[ln].tokens.is_empty() {
            tokenize_line_no(&mut texts[ln]);
        }

        loop {
            tkn_ix += 1;
            if let Some(tkn) = texts[ln].tokens.get(tkn_ix as usize).cloned() {
                trace!("\t{}: {}-{}", ln, tkn.byte_start, tkn.byte_end);
                if predicate(tkn.clone()) {
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
    mut tokenize_line_no: impl FnMut(&mut LineData),
) -> CursorContext {
    debug!("classify_complesion_mode({},{})", line_no, char_offset);
    let (line_no, cursor_ix, cursor_tkn) = {
        let (l, i, c) = cursor_tkn(texts, line_no, char_offset, &mut tokenize_line_no);
        (l, i, c.clone())
    };

    cursor_tkn.as_ref().inspect(|t| {
        trace!(
            "current: {:?},{:?},{:?}",
            t.details[6].as_str(),
            t.details[0..=3].to_vec(),
            t.tag
        )
    });

    // カーソル前の最後の非空白トークンを後方探索
    let (_, _, before_tkn) = before_token(texts, line_no, cursor_ix, tokenize_line_no, |tkn| {
        !is_whitespace(&tkn)
    });

    before_tkn.as_ref().inspect(|t| {
        trace!(
            "before: {:?},{:?},{:?}",
            t.details[6].as_str(),
            t.details[0..=3].to_vec(),
            t.tag
        )
    });

    // tag ~~ とdepth ~~ を使って括弧の内外を判定
    // - tag == InBracket: 通常のトークンが括弧内にある
    // - is_bracket_open: 開き括弧の直後（自身はNormalだが直後は括弧内）
    // - depth > 0: ネストした括弧閉の直後でまだ外側の括弧内にいる場合
    let in_bracket = if let Some(tkn) = before_tkn.as_ref() {
        tkn.tag == TokenStatus::InBracket || is_bracket_open(tkn)
    } else {
        false
    };

    if !in_bracket {
        if before_tkn.as_ref().is_some_and(is_bracket_close) {
            return CursorContext::AfterClosingBracket;
        }
        if before_tkn.as_ref().is_some_and(is_sentence_end) {
            return CursorContext::AfterSentenceEnd;
        }
        return CursorContext::Other;
    }

    // 括弧内
    if before_tkn.as_ref().is_some_and(is_bracket_open)
        && cursor_tkn.as_ref().is_some_and(is_bracket_close)
    {
        return CursorContext::EmptyBracket;
    }
    if cursor_tkn.as_ref().is_some_and(is_bracket_close) {
        return CursorContext::BeforeClosingBracket;
    }
    CursorContext::InBracketOther
}

/// カーソル位置(`line_no`, `char_offset`)が指すトークンを、フォールバック無しで返す。
///
/// `char_offset` は LSP の `Position.character` をそのまま受け取る想定。サーバーは
/// `positionEncoding: UTF8` を宣言しているが、実クライアント(Zed)は常に UTF-16 コード単位で
/// Position を送ってくる。日本語の常用文字は BMP 内で UTF-16 コード単位と Rust の `char` が
/// 1:1 一致するため、`.chars().take(char_offset)` による char 基準の変換で実際には正しく動く
/// (補助面文字が絡む場合はズレうるが、既知の別課題として扱う)。
///
/// 見つからなければ `None`。次のトークンへのフォールバックが必要な呼び出し元(補完)は
/// `cursor_tkn` を使うこと。
pub fn token_at(
    texts: &mut [LineData],
    line_no: usize,
    char_offset: usize,
    tokenize_line_no: &mut impl FnMut(&mut LineData),
) -> Option<(usize, CachedLinderaToken)> {
    // カーソル位置のトークンを取得
    if !texts[line_no].text.is_empty() && texts[line_no].tokens.is_empty() {
        tokenize_line_no(&mut texts[line_no]);
    }
    let byte_offset: usize = texts[line_no]
        .text
        .chars()
        .take(char_offset)
        .fold(0, |a, c| a + c.len_utf8());

    trace!(
        "line_no: {}, char_offset: {}, byte_offset: {}",
        line_no, char_offset, byte_offset
    );
    trace!("{}", texts[line_no].text.chars().take(45).collect::<String>());

    texts[line_no]
        .tokens // Linderaは半角スペースをtokenにしない
        .iter()
        .enumerate()
        .find(|(_ix, tkn)| tkn.byte_start <= byte_offset && byte_offset < tkn.byte_end)
        .map(|(ix, tkn)| (ix, tkn.clone()))
}

fn cursor_tkn(
    texts: &mut [LineData],
    line_no: usize,
    char_offset: usize,
    tokenize_line_no: &mut impl FnMut(&mut LineData),
) -> (usize, usize, Option<CachedLinderaToken>) {
    debug!("cursor_tkn");

    let find_result = token_at(texts, line_no, char_offset, tokenize_line_no)
        .map(|(ix, tkn)| (ix, Some(tkn)));

    let ix_at_end = texts[line_no].tokens.len();

    let (cursor_ix, cursor_tkn) = find_result.unwrap_or_else(|| {
        debug!("Not found");
        if let Some(tkn) = next_token(texts, line_no, ix_at_end, tokenize_line_no, |tkn| {
            !is_whitespace(&tkn)
        }) {
            (ix_at_end, Some(tkn))
        } else {
            (ix_at_end, None)
        }
    });
    (line_no, cursor_ix, cursor_tkn)
}

fn is_end_of_sentence(tkn: &CachedLinderaToken) -> bool {
    tkn.details[0] == "記号"
        && match tkn.details.get(1).map(|s| s.as_str()) {
            Some("句点") | //=> true,
            Some("括弧閉") => true,
            _ => false,
        }
}

pub fn before_sentences_upto(
    texts: &DashMap<String, Vec<LineData>>,
    uri: &str,
    line_no: usize,
    char_offset: usize,
    len: usize,
    mut tokenize_line_no: impl FnMut(usize),
) -> Vec<String> {
    debug!(
        "before_sentences_upto: line_no={}, char_offset={}, len={}",
        line_no, char_offset, len
    );

    let mut text = texts.get(uri).unwrap();

    // char_offsetrからtoken_indexに変換
    if text[line_no].tokens.is_empty() {
        drop(text);
        tokenize_line_no(line_no);
        text = texts.get(uri).unwrap();

        debug!(
            "{:?} {:?}",
            text[line_no].text.chars().take(20).collect::<String>(),
            text[line_no].tokens.first()
        );
    }
    let mut line_buf = String::new();

    let mut last_byte = text[line_no]
        .text
        .chars()
        .take(char_offset)
        .fold(0, |acc, c| acc + c.len_utf8());

    let mut tkn_ix: i64 = if let Some((token_index, tkn)) = text[line_no]
        .tokens
        .iter()
        .enumerate()
        .find(|(_, tkn)| tkn.byte_start <= last_byte && last_byte < tkn.byte_end)
    {
        line_buf.push_str(&text[line_no].text[tkn.byte_start..tkn.byte_end]);
        last_byte = tkn.byte_end;
        token_index as i64
    } else {
        text[line_no].tokens.len() as i64
    };

    let mut ln = line_no as i64;
    let mut result = vec![];

    'outer: loop {
        // 行のループ
        let tmp = text.get(ln as usize);
        let Some(mut line) = tmp else {
            break;
        };
        if line.tokens.is_empty() {
            let _ = line;
            drop(text);
            tokenize_line_no(ln as usize);
            text = texts.get(uri).unwrap();
            line = text.get(ln as usize).unwrap();
        }

        if tkn_ix > line.tokens.len() as i64 {
            tkn_ix = line.tokens.len() as i64;
            last_byte = line.text.len()
        }
        let (token_index, tkn) = before_token_inline(line, tkn_ix as usize, |_| true);

        last_byte = if let Some(tkn) = tkn {
            line_buf.push_str(&line.text[tkn.byte_start..last_byte]);
            tkn.byte_start
        } else {
            line.text.len()
        };
        tkn_ix = token_index as i64;
        trace!("\t{},({} / {})", ln, tkn_ix, line.tokens.len() as i64);

        loop {
            // トークンのループ
            tkn_ix -= 1;
            trace!("\t\t{}", tkn_ix);
            if let Some(tkn) = line.tokens.get(tkn_ix as usize) {
                trace!("\t{}: {}-{}", tkn.details[6], tkn.byte_start, tkn.byte_end);
                if is_end_of_sentence(tkn) {
                    debug!("\tnext_sentence {:?},{}", line_buf, result.len());
                    result.insert(0, line_buf.clone());
                    line_buf.clear();
                    if result.len() >= len {
                        break 'outer;
                    }
                }
                line_buf.insert_str(0, &line.text[tkn.byte_start..last_byte]);
                last_byte = tkn.byte_start;
            } else {
                ln -= 1;
                tkn_ix = i64::MAX;
                debug!("\tnext line {:?}", line_buf);
                result.insert(0, line_buf.clone());
                line_buf.clear();
                break;
            }
        }
    }

    if !line_buf.is_empty() {
        debug!("\tend of text{:?},{}", line_buf, result.len());
        result.insert(0, line_buf);
    }

    result
}

#[cfg(test)]
mod tests {
    use crate::CursorContext;
    use crate::Highlighter;
    use crate::LineData;
    use crate::cursor_context::before_sentences_upto;
    use crate::cursor_context::classify_complesion_mode;
    use crate::cursor_context::token_at;
    // use indoc::indoc;
    use regex::Regex;
    use std::str::FromStr;

    fn lines(text: &str) -> Vec<LineData> {
        let cr = Regex::new(r"\r\n|\r|\n").unwrap();
        cr.split(text)
            .map(|l| LineData::from_str(l).unwrap())
            .collect()
    }

    struct TestData {
        hl: Highlighter,
    }

    impl TestData {
        fn new() -> Self {
            Self {
                hl: Highlighter::new(),
            }
        }

        fn tokenize(&self, line: &mut LineData) {
            self.hl.tokenize(line);
        }
    }

    // fn tokenize(text: &str) -> (Vec<LineData>, Highlighter) {
    //     let hl = Highlighter::new();
    //     let cr = Regex::new(r"\r\n|\r|\n").unwrap();
    //     let line: Vec<LineData> = cr
    //         .split(text)
    //         .map(|l| LineData::from_str(l).unwrap())
    //         .collect();
    //     (line, hl)
    // }

    // pub fn tokenize_line(hl: &Highlighter, texts: &mut Vec<LineData>, line_no: usize) {
    //     if !texts[line_no].tokens.is_empty() {
    //         return;
    //     }
    //     hl.tokenize(texts.get_mut(line_no).unwrap());
    // }

    #[test]
    fn after_closing_bracket() {
        let mut texts = lines("彼は言った。「こんにちは」");
        let td = TestData::new();
        let offset = texts[0].text.chars().count(); // カーソルは末尾
        assert_eq!(
            classify_complesion_mode(&mut texts, 0, offset, |line| td.tokenize(line)),
            CursorContext::AfterClosingBracket
        );
    }

    #[test]
    fn after_closing_bracket_with_whitespace() {
        let mut text = lines("思い出した。\n「」\n　故郷たる");
        let td = TestData::new();
        assert_eq!(
            classify_complesion_mode(&mut text, 2, 1, |line| td.tokenize(line)),
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
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn after_sentence_end_with_newline() {
        let mut text = lines("これは文章。\n");
        let td = TestData::new();
        assert_eq!(
            classify_complesion_mode(&mut text, 1, 0, |line| td.tokenize(line)),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn empty_bracket() {
        let mut text = lines("「」");
        let td = TestData::new();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 1, |line| td.tokenize(line)),
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
        let td = TestData::new();
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn before_closing_bracket_multiline() {
        let mut text = lines("「こんにちは\n」");
        let td = TestData::new();
        // カーソルは1行目の末尾
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn in_bracket_other() {
        let mut text = lines("「こんにちは、");
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::InBracketOther
        );
    }

    #[test]
    fn other_mid_sentence() {
        let mut text = lines("これは途中");
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::Other
        );
    }

    #[test]
    fn other_top_sentence() {
        let mut text = lines("　これは段落頭");
        let td = TestData::new();
        let offset = 0;
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::Other
        );
    }

    #[test]
    fn other_empty_document() {
        let mut text = lines("");
        let td = TestData::new();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, |line| td.tokenize(line)),
            CursorContext::Other
        );
    }

    #[test]
    fn nested_brackets() {
        // 「『内側』|外側」 → depth=1なのでInBracketOther
        let mut text = lines("「『内側』外側」");
        let td = TestData::new();
        let offset = "「『内側』外側".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn after_nested_inner_close() {
        // 「『内側』|」 → depth=1, lastが括弧閉(内側の)だが外側はまだ開いている
        // → next_significantが」 → BeforeClosingBracket
        let mut text = lines("「『内側』」");
        let td = TestData::new();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, 0, |line| td.tokenize(line)),
            CursorContext::Other
        );

        let offset = "「『内側』".chars().count();
        assert_eq!(
            classify_complesion_mode(&mut text, 0, offset, |line| td.tokenize(line)),
            CursorContext::BeforeClosingBracket
        );
    }
    /*
        #[test]
        fn sentences_upto_single() {
            let (mut texts, _) = tokenize("一文目です。");
            let offset = texts[0].text.chars().count();
            let result = before_sentences_upto(&mut texts, 0, offset, 10, |_| {});
            assert_eq!(result.len(), 1);
            assert!(result.iter().any(|s| s.contains("。")));
        }

        #[test]
        fn sentences_upto_multiple() {
            let (mut texts, _) = tokenize("一文目です。二文目です。");
            let offset = texts[0].text.chars().count();
            let result = before_sentences_upto(&mut texts, 0, offset, 10, |_| {});
            assert_eq!(result.len(), 2);
        }

        #[test]
        fn sentences_upto_len_limit() {
            let (mut texts, _) = tokenize("一文目。二文目。三文目。");
            let offset = texts[0].text.chars().count();
            let result = before_sentences_upto(&mut texts, 0, offset, 2, |_| {});
            assert_eq!(result.len(), 2);
        }

        #[test]
        fn sentences_upto_multiline() {
            let (mut texts, _) = tokenize("一文目。\n\n二文目。");
            let offset = texts[2].text.chars().count();
            // 0行目末尾のカーソル → 0行目の文のみ
            let result = before_sentences_upto(&mut texts, 2, offset, 10, |_| {});
            assert_eq!(result.len(), 3);
        }
    */
    // #[test]
    // fn sentences_upto_multiline2() {
    //     let (mut texts, _) = tokenize(indoc!(
    //         "通信回線を開いた。やがてスピーカーから微かな通信音が響き渡った。
    //         「応答せよ、グラナダ管制、これより進入を開始する」
    //         「こちらサイド・スリー防衛艦隊所属、認識番号を確認されたし」"
    //     ));
    //     let offset = texts[2].text.chars().count() - 1;
    //     let result = before_sentences_upto(&mut texts, 2, offset, 10, |_| {});
    //     assert_eq!(result.len(), 4);
    // }

    #[test]
    fn test_token_at_hit() {
        // "田中" は Lindera IPADIC で単一トークン(固有名詞,人名,姓)になる(実測確認済み)。
        let mut texts = lines("田中は歩いた。");
        let td = TestData::new();
        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        // char_offset=1 は "田中" トークンの内部(先頭文字の直後)
        let hit = token_at(&mut texts, 0, 1, &mut tokenize);
        let (_ix, tkn) = hit.expect("token should be found");
        assert_eq!(&texts[0].text[tkn.byte_start..tkn.byte_end], "田中");
    }

    #[test]
    fn test_token_at_miss_no_fallback() {
        // 行末より後ろの char_offset では、cursor_tkn と違いフォールバックせず None を返す。
        let mut texts = lines("田中");
        let td = TestData::new();
        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let len = texts[0].text.chars().count();
        let hit = token_at(&mut texts, 0, len + 10, &mut tokenize);
        assert!(hit.is_none(), "{:?}", hit);
    }

    #[test]
    fn test_token_at_multibyte_boundary() {
        // "田中" は2文字(6バイト)。char_offset=1(バイト3、"田中"内部)は "田中" に、
        // char_offset=2(バイト6、"田中"の直後)は次のトークンにヒットすることを確認する
        // (全角文字をバイト単位ではなく文字単位で正しく境界判定できていること)。
        let mut texts = lines("田中は歩いた。");
        let td = TestData::new();

        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let inside = token_at(&mut texts, 0, 1, &mut tokenize)
            .expect("char_offset=1 should hit a token");
        assert_eq!(&texts[0].text[inside.1.byte_start..inside.1.byte_end], "田中");

        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let after = token_at(&mut texts, 0, 2, &mut tokenize)
            .expect("char_offset=2 should hit the next token");
        assert_ne!(
            (after.1.byte_start, after.1.byte_end),
            (inside.1.byte_start, inside.1.byte_end),
            "「田中」の直後は別トークンにヒットするはず"
        );
        assert_eq!(after.1.byte_start, inside.1.byte_end, "境界が「田中」の直後(バイト6)であること");
    }
}
