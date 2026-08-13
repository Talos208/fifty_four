use crate::types::{CachedLinderaToken, CursorContext, LineData, TokenStatus};
use dashmap::DashMap;
#[allow(unused_imports)]
use log::{debug, error, trace};
use std::cmp::max;
use std::cmp::min;
use tower_lsp_server::lsp_types::{Position, Range};
use tracing::instrument;

#[instrument]
fn is_bracket_open(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("括弧開")
}

#[instrument]
fn is_bracket_close(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("括弧閉")
}

#[instrument]
fn is_sentence_end(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("句点")
}

#[instrument]
fn is_whitespace(token: &CachedLinderaToken) -> bool {
    token.details[0] == "記号" && token.details.get(1).map(|s| s.as_str()) == Some("空白")
}

#[instrument(skip(predicate))]
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

#[instrument(skip(texts, tokenize_line_no, predicate))]
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

#[instrument(skip(texts, tokenize_line_no, predicate))]
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

#[instrument(skip(texts, tokenize_line_no))]
pub fn classify_complesion_mode(
    texts: &mut [LineData],
    line_no: usize,
    utf16_offset: usize,
    mut tokenize_line_no: impl FnMut(&mut LineData),
) -> CursorContext {
    debug!("classify_complesion_mode({},{})", line_no, utf16_offset);
    let (line_no, cursor_ix, cursor_tkn) = {
        let (l, i, c) = cursor_tkn(texts, line_no, utf16_offset, &mut tokenize_line_no);
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

/// カーソル位置(`line_no`, `utf16_offset`)を含む「文」の範囲を返す(code action の
/// 対象範囲決定に使う。選択範囲が無いときのフォールバック)。
///
/// 文の境界は `is_end_of_sentence`(句点 or 括弧閉、`classify_complesion_mode` と同じ基準)。
/// - 開始: カーソルより前方の直近の文末トークンの直後(無ければ文書先頭)
/// - 終了: カーソル位置以降(カーソル自身のトークンを含む)の最初の文末トークンの直後、
///   その記号自体を含む(無ければ文書末尾)
#[instrument(skip(texts, tokenize_line_no))]
pub(crate) fn sentence_range_at(
    texts: &mut [LineData],
    line_no: usize,
    utf16_offset: usize,
    mut tokenize_line_no: impl FnMut(&mut LineData),
) -> Range {
    if texts[line_no].tokens.is_empty() && !texts[line_no].text.is_empty() {
        tokenize_line_no(&mut texts[line_no]);
    }
    let cursor_byte = crate::types::utf16_to_byte_offset(&texts[line_no].text, utf16_offset);
    // カーソルを含む、またはカーソル以降の最初のトークンの index。
    let cursor_ix = texts[line_no]
        .tokens
        .iter()
        .position(|t| t.byte_end > cursor_byte)
        .unwrap_or(texts[line_no].tokens.len());

    // 開始: カーソルより前方を後方探索(before_token は token_index 未満のみを見るため
    // カーソル自身のトークンは対象に含まれない)。
    let (start_line, start_byte) = {
        let (ln, _ix, tkn) = before_token(texts, line_no, cursor_ix, &mut tokenize_line_no, |t| {
            is_end_of_sentence(&t)
        });
        match tkn {
            Some(t) => (ln, t.byte_end),
            None => (0, 0),
        }
    };

    // 終了: カーソル位置(自身のトークン含む)から前方探索。
    let (end_line, end_byte) = {
        let mut ln = line_no;
        let mut ix = cursor_ix;
        loop {
            if texts.get(ln).is_none() {
                let last = texts.len().saturating_sub(1);
                break (last, texts.get(last).map(|l| l.text.len()).unwrap_or(0));
            }
            if texts[ln].tokens.is_empty() && !texts[ln].text.is_empty() {
                tokenize_line_no(&mut texts[ln]);
            }
            if let Some(tkn) = texts[ln].tokens.get(ix).cloned() {
                if is_end_of_sentence(&tkn) {
                    break (ln, tkn.byte_end);
                }
                ix += 1;
            } else {
                ln += 1;
                ix = 0;
            }
        }
    };

    let start_char = crate::types::utf16_len(&texts[start_line].text[..start_byte]) as u32;
    let end_char = crate::types::utf16_len(&texts[end_line].text[..end_byte]) as u32;

    Range::new(
        Position::new(start_line as u32, start_char),
        Position::new(end_line as u32, end_char),
    )
}

/// カーソル位置(`line_no`, `utf16_offset`)が指すトークンを、フォールバック無しで返す。
///
/// `utf16_offset` は LSP の `Position.character` をそのまま受け取る想定
/// (`positionEncoding: UTF16` を宣言しているため UTF-16 コード単位)。
///
/// 見つからなければ `None`。次のトークンへのフォールバックが必要な呼び出し元(補完)は
/// `cursor_tkn` を使うこと。
#[instrument(skip(texts, tokenize_line_no))]
pub fn token_at(
    texts: &mut [LineData],
    line_no: usize,
    utf16_offset: usize,
    tokenize_line_no: &mut impl FnMut(&mut LineData),
) -> Option<(usize, CachedLinderaToken)> {
    // カーソル位置のトークンを取得
    if !texts[line_no].text.is_empty() && texts[line_no].tokens.is_empty() {
        tokenize_line_no(&mut texts[line_no]);
    }
    let byte_offset = crate::types::utf16_to_byte_offset(&texts[line_no].text, utf16_offset);

    trace!(
        "line_no: {}, utf16_offset: {}, byte_offset: {}",
        line_no, utf16_offset, byte_offset
    );
    trace!(
        "{}",
        texts[line_no].text.chars().take(45).collect::<String>()
    );

    texts[line_no]
        .tokens // Linderaは半角スペースをtokenにしない
        .iter()
        .enumerate()
        .find(|(_ix, tkn)| tkn.byte_start <= byte_offset && byte_offset < tkn.byte_end)
        .map(|(ix, tkn)| (ix, tkn.clone()))
}

#[instrument(skip(texts, tokenize_line_no))]
fn cursor_tkn(
    texts: &mut [LineData],
    line_no: usize,
    utf16_offset: usize,
    tokenize_line_no: &mut impl FnMut(&mut LineData),
) -> (usize, usize, Option<CachedLinderaToken>) {
    debug!("cursor_tkn");

    let find_result =
        token_at(texts, line_no, utf16_offset, tokenize_line_no).map(|(ix, tkn)| (ix, Some(tkn)));

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

#[instrument(skip(texts, tokenize_line_no))]
pub fn before_sentences_upto(
    texts: &DashMap<String, Vec<LineData>>,
    uri: &str,
    line_no: usize,
    utf16_offset: usize,
    len: usize,
    mut tokenize_line_no: impl FnMut(usize),
) -> Vec<String> {
    debug!(
        "before_sentences_upto: line_no={}, utf16_offset={}, len={}",
        line_no, utf16_offset, len
    );

    let mut text = texts.get(uri).unwrap();

    // utf16_offsetからtoken_indexに変換
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

    let mut last_byte = crate::types::utf16_to_byte_offset(&text[line_no].text, utf16_offset);

    // カーソルがトークン上(境界含む)にある場合でも、ここではそのトークンを consume しない。
    // last_byte をカーソル位置のままにしておけば、直後の 'outer ループが
    // [直前トークンの byte_start .. カーソル位置) をまとめて取り込むため、
    // カーソルより後方の文字の混入やトークンの二重計上なしに「カーソル直前まで」が集まる。
    // (以前はカーソルトークン全体を push + last_byte を byte_end へ進めていたため、
    //  閉じ括弧直前などトークン境界にカーソルがあると後方文字の混入・重複が起きていた)
    let mut tkn_ix: i64 = text[line_no]
        .tokens
        .iter()
        .position(|tkn| tkn.byte_start <= last_byte && last_byte < tkn.byte_end)
        .map(|ix| ix as i64)
        .unwrap_or_else(|| {
            // last_byte を含むトークンが無い場合(行頭の空白の直前、行末など)。
            // last_byte 以前に完全に収まっているトークンの数を数えれば、それが
            // 「カーソルの直前にあるトークンの次のインデックス」になる。
            // カーソルが最初のトークンより前(行頭の空白等)なら 0 に、
            // 最後のトークンより後(行末)なら tokens.len() になり、どちらの端でも
            // 正しく振る舞う。以前は無条件に tokens.len()(行末扱い)へ倒していたため、
            // 行頭で一致しないケースを行末と誤認し、後段のスライスで
            // `line.text[行末トークンのbyte_start..last_byte(=0)]` という
            // 逆転レンジを作ってpanicしていた。
            text[line_no]
                .tokens
                .iter()
                .filter(|tkn| tkn.byte_end <= last_byte)
                .count() as i64
        });

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
            // ここに来るのは tkn_ix が 0 のとき(`before_token_inline` は
            // token_index=0 の場合のみ None を返す)。この時点で last_byte は
            // 既に正しい値になっている(行頭でのカーソル位置、または直前の
            // 行またぎ時のクランプによる line.text.len())ので上書きしない。
            last_byte
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

/// LLM の生レスポンスを、候補行の列へ分割する。
///
/// モデルはまれに「これから挙げる候補の意図」を独り言のように書き出してから
/// 空行を挟んで実際の候補を続けることがある(観測例: 意図説明2行 + 空行 + 実際の
/// 候補3行、という計6行の応答)。これをそのまま `.lines()` するとダミー候補が
/// 混入し、空行自体も `decorate_candidate` の既定分岐で「。」だけの幽霊候補になる。
///
/// このため、応答内に空行があれば**最後の空行より後ろ**を実際の候補群とみなし、
/// そこに実質的な行が1つでもあればそちらを採用する(モデルが前置きと本題を
/// 空行で区切る、という観測された振る舞いに対する経験則)。空行が無い、または
/// 空行の後ろに実質的な行が無い(末尾の空行のみ等)場合は、全体から空行だけを
/// 除いたものを返す。
#[instrument]
pub(crate) fn extract_candidate_lines(response: &str) -> Vec<&str> {
    let lines: Vec<&str> = response.lines().collect();
    let last_blank = lines.iter().rposition(|l| l.trim().is_empty());
    let segment: &[&str] = match last_blank {
        Some(idx) if lines[idx + 1..].iter().any(|l| !l.trim().is_empty()) => {
            debug!("Irregal response: {:?}", lines[0..idx].join("\\n"));
            &lines[idx + 1..]
        }
        _ => &lines[..],
    };
    segment
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// 前文の末尾がこの文字なら、続く候補の先頭に句点「。」を前置しない。
/// 読点・句点・感嘆符等の直後に句点を重ねると不自然になるほか、
/// 開き括弧の直後(会話の書き出し)にも句点は不要。
#[instrument]
fn ends_with_no_period_needed(c: char) -> bool {
    matches!(c, '、' | '。' | '！' | '？' | '…' | '―' | '「' | '『')
}

/// LLM の生候補(1行)を、カーソル文脈と直前テキストの末尾文字に応じて整形する。
///
/// `prev_tail` はカーソル直前の文字(=候補の直前に来る文字。無ければ `None`)。
/// `BeforeClosingBracket`(閉じ括弧の直前)では、句点の付与/除去を独立した
/// 2ステップとして扱う:
///   1. 候補末尾の「。」は常に除去する(`」` の直前に句点は置かない慣習のため)
///   2. 候補先頭への「。」前置は、前文が読点等で終わっていない場合のみ行う
/// 以前は if/else if/else の排他分岐だったため、候補が「。」で終わる場合に
/// 末尾除去が働かず `。わかった。` のような二重句点が生じていた。
#[instrument]
pub(crate) fn decorate_candidate(
    context: CursorContext,
    raw: &str,
    prev_tail: Option<char>,
) -> String {
    match context {
        CursorContext::BeforeClosingBracket => {
            let trimmed = raw.strip_suffix('。').unwrap_or(raw);
            let needs_period =
                !trimmed.starts_with('。') && !prev_tail.is_some_and(ends_with_no_period_needed);
            if needs_period {
                format!("。{trimmed}")
            } else {
                trimmed.to_string()
            }
        }
        CursorContext::EmptyBracket => {
            if let Some(t) = raw.strip_suffix('。') {
                t.to_string()
            } else {
                raw.to_string()
            }
        }
        CursorContext::AfterClosingBracket => "\n".to_string() + raw,
        _ => {
            if !raw.ends_with('。') {
                raw.to_string() + "。"
            } else {
                raw.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cursor_context::before_sentences_upto;
    use crate::cursor_context::classify_complesion_mode;
    use crate::cursor_context::token_at;
    use crate::highlight::Highlighter;
    use crate::types::{CursorContext, LineData};
    use dashmap::DashMap;
    use tower_lsp_server::lsp_types::{Position, Range};
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
            self.hl.tokenize(line, &std::collections::HashSet::new());
        }

        /// 本番(completion)と同じ手順で分類する:
        /// カーソル行まで括弧深さを畳み込んでから classify_complesion_mode を呼ぶ。
        fn classify(
            &self,
            texts: &mut Vec<LineData>,
            line_no: usize,
            offset: usize,
        ) -> CursorContext {
            self.hl.ensure_bracket_depth(texts.as_mut_slice(), line_no);
            classify_complesion_mode(texts, line_no, offset, |line| self.tokenize(line))
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
            td.classify(&mut texts, 0, offset),
            CursorContext::AfterClosingBracket
        );
    }

    #[test]
    fn after_closing_bracket_with_whitespace() {
        let mut text = lines("思い出した。\n「」\n　故郷たる");
        let td = TestData::new();
        assert_eq!(
            td.classify(&mut text, 2, 1),
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
            td.classify(&mut text, 0, offset),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn after_sentence_end_with_newline() {
        let mut text = lines("これは文章。\n");
        let td = TestData::new();
        assert_eq!(
            td.classify(&mut text, 1, 0),
            CursorContext::AfterSentenceEnd
        );
    }

    #[test]
    fn empty_bracket() {
        let mut text = lines("「」");
        let td = TestData::new();
        assert_eq!(td.classify(&mut text, 0, 1), CursorContext::EmptyBracket);
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
            td.classify(&mut text, 0, offset),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn before_closing_bracket_consecutive_dialogue_lines() {
        // 連続する台詞行の1行目、」の直前 → BeforeClosingBracket
        // (修正前: completion 経路では tag が常に Normal だったため Other に誤判定されていた)
        let mut text = lines("「セリフ１」\n「セリフ２」");
        let td = TestData::new();
        let offset = "「セリフ１".chars().count();
        assert_eq!(
            td.classify(&mut text, 0, offset),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn in_bracket_other_second_dialogue_line() {
        // 連続台詞の2行目、読点の直後(台詞の途中) → InBracketOther
        let mut text = lines("「セリフ１」\n「セリフ２、");
        let td = TestData::new();
        let offset = "「セリフ２、".chars().count();
        assert_eq!(
            td.classify(&mut text, 1, offset),
            CursorContext::InBracketOther
        );
    }

    #[test]
    fn before_closing_bracket_multiline() {
        let mut text = lines("「こんにちは\n」");
        let td = TestData::new();
        // カーソルは1行目の末尾
        let offset = "「こんにちは".chars().count();
        assert_eq!(
            td.classify(&mut text, 0, offset),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn in_bracket_other() {
        let mut text = lines("「こんにちは、");
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        assert_eq!(
            td.classify(&mut text, 0, offset),
            CursorContext::InBracketOther
        );
    }

    #[test]
    fn other_mid_sentence() {
        let mut text = lines("これは途中");
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        assert_eq!(td.classify(&mut text, 0, offset), CursorContext::Other);
    }

    #[test]
    fn other_top_sentence() {
        let mut text = lines("　これは段落頭");
        let td = TestData::new();
        let offset = 0;
        assert_eq!(td.classify(&mut text, 0, offset), CursorContext::Other);
    }

    #[test]
    fn other_empty_document() {
        let mut text = lines("");
        let td = TestData::new();
        assert_eq!(td.classify(&mut text, 0, 0), CursorContext::Other);
    }

    #[test]
    fn nested_brackets() {
        // 「『内側』|外側」 → depth=1なのでInBracketOther
        let mut text = lines("「『内側』外側」");
        let td = TestData::new();
        let offset = "「『内側』外側".chars().count();
        assert_eq!(
            td.classify(&mut text, 0, offset),
            CursorContext::BeforeClosingBracket
        );
    }

    #[test]
    fn after_nested_inner_close() {
        // 「『内側』|」 → depth=1, lastが括弧閉(内側の)だが外側はまだ開いている
        // → next_significantが」 → BeforeClosingBracket
        let mut text = lines("「『内側』」");
        let td = TestData::new();
        assert_eq!(td.classify(&mut text, 0, 0), CursorContext::Other);

        let offset = "「『内側』".chars().count();
        assert_eq!(
            td.classify(&mut text, 0, offset),
            CursorContext::BeforeClosingBracket
        );
    }
    // ─── before_sentences_upto ───────────────────────────────────────────

    /// テスト用: uri "uri" 1本の DashMap を組み立てる。
    fn dash(text: &str) -> DashMap<String, Vec<LineData>> {
        let m = DashMap::new();
        m.insert("uri".to_string(), lines(text));
        m
    }

    /// テスト用: 本番(main.rs completion)と同じ遅延トークン化クロージャで呼び出す。
    fn sentences(
        td: &TestData,
        texts: &DashMap<String, Vec<LineData>>,
        line_no: usize,
        utf16_offset: usize,
        len: usize,
    ) -> Vec<String> {
        before_sentences_upto(texts, "uri", line_no, utf16_offset, len, |ln| {
            let mut t = texts.get_mut("uri").unwrap();
            if let Some(l) = t.get_mut(ln) {
                td.tokenize(l);
            }
        })
    }

    #[test]
    fn sentences_upto_cursor_before_closing_bracket() {
        // 連続台詞の1行目、閉じ括弧」の直前にカーソルがあるとき、
        // カーソルより後方の」が混入したり、文字が重複したりしないこと。
        // (カーソルのバイト位置が次トークンの byte_start と一致する境界ケース)
        let td = TestData::new();
        let texts = dash("「セリフ１」\n「セリフ２」");
        let offset = "「セリフ１".chars().count();
        let result = sentences(&td, &texts, 0, offset, 10);
        assert_eq!(result.concat(), "「セリフ１", "{:?}", result);
    }

    #[test]
    fn sentences_upto_cursor_mid_token() {
        // トークン内部(部分入力済みの語の途中)にカーソルがある場合も、
        // カーソルより後方の文字が混入しないこと。
        let td = TestData::new();
        let texts = dash("これは途中");
        let offset = "これは途".chars().count(); // "途中" トークンの内部
        let result = sentences(&td, &texts, 0, offset, 10);
        assert_eq!(result.concat(), "これは途", "{:?}", result);
    }

    #[test]
    fn sentences_upto_multiple_sentences() {
        let td = TestData::new();
        let texts = dash("一文目です。二文目です。");
        let offset = "一文目です。二文目です。".chars().count();
        let result = sentences(&td, &texts, 0, offset, 10);
        assert_eq!(result.len(), 2, "{:?}", result);
        assert_eq!(result.concat(), "一文目です。二文目です。");
    }

    #[test]
    fn sentences_upto_len_limit() {
        let td = TestData::new();
        let texts = dash("一文目。二文目。三文目。");
        let offset = "一文目。二文目。三文目。".chars().count();
        let result = sentences(&td, &texts, 0, offset, 2);
        assert_eq!(result.len(), 2, "{:?}", result);
        // 直近の2文だけが返る
        assert_eq!(result.concat(), "二文目。三文目。");
    }

    #[test]
    fn sentences_upto_cursor_at_start_of_line_with_leading_space() {
        // カーソルが行頭(全角スペースの直前)にあり、その行の実質的な最初の
        // トークンが byte_start=0 でない場合、last_byte(=0)を含むトークンが
        // 見つからない。以前はこれを「行末」と誤認し、行の最後のトークンとの
        // 逆転レンジでpanicしていた(実機で確認されたクラッシュの再現)。
        let td = TestData::new();
        let texts = dash("前の行の文。\n　艦艇類別等級表には、ただ「戦艦」とある。");
        let result = sentences(&td, &texts, 1, 0, 10);
        // 行頭カーソルなので2行目からは何も取れず、1行目の内容だけが返る
        assert_eq!(result.concat(), "前の行の文。", "{:?}", result);
    }

    #[test]
    fn sentences_upto_consecutive_dialogue_lines() {
        // 連続台詞2行、カーソルは2行目末尾 → 両方の台詞が行単位で返る
        let td = TestData::new();
        let texts = dash("「セリフ１」\n「セリフ２」");
        let offset = "「セリフ２」".chars().count();
        let result = sentences(&td, &texts, 1, offset, 10);
        assert_eq!(result.concat(), "「セリフ１」「セリフ２」", "{:?}", result);
    }

    #[test]
    fn test_token_at_hit() {
        // "田中" は Lindera IPADIC で単一トークン(固有名詞,人名,姓)になる(実測確認済み)。
        let mut texts = lines("田中は歩いた。");
        let td = TestData::new();
        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        // utf16_offset=1 は "田中" トークンの内部(先頭文字の直後、田はBMPなので1u16)
        let hit = token_at(&mut texts, 0, 1, &mut tokenize);
        let (_ix, tkn) = hit.expect("token should be found");
        assert_eq!(&texts[0].text[tkn.byte_start..tkn.byte_end], "田中");
    }

    #[test]
    fn test_token_at_miss_no_fallback() {
        // 行末より後ろの utf16_offset では、cursor_tkn と違いフォールバックせず None を返す。
        let mut texts = lines("田中");
        let td = TestData::new();
        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let len = crate::types::utf16_len(&texts[0].text);
        let hit = token_at(&mut texts, 0, len + 10, &mut tokenize);
        assert!(hit.is_none(), "{:?}", hit);
    }

    #[test]
    fn test_token_at_multibyte_boundary() {
        // "田中" は2文字(6バイト、BMPなのでUTF-16でも2単位)。utf16_offset=1(バイト3、
        // "田中"内部)は "田中" に、utf16_offset=2(バイト6、"田中"の直後)は次のトークンに
        // ヒットすることを確認する(全角文字をバイト単位ではなく文字単位で正しく境界判定
        // できていること)。
        let mut texts = lines("田中は歩いた。");
        let td = TestData::new();

        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let inside =
            token_at(&mut texts, 0, 1, &mut tokenize).expect("utf16_offset=1 should hit a token");
        assert_eq!(
            &texts[0].text[inside.1.byte_start..inside.1.byte_end],
            "田中"
        );

        let mut tokenize = |line: &mut LineData| td.tokenize(line);
        let after = token_at(&mut texts, 0, 2, &mut tokenize)
            .expect("utf16_offset=2 should hit the next token");
        assert_ne!(
            (after.1.byte_start, after.1.byte_end),
            (inside.1.byte_start, inside.1.byte_end),
            "「田中」の直後は別トークンにヒットするはず"
        );
        assert_eq!(
            after.1.byte_start, inside.1.byte_end,
            "境界が「田中」の直後(バイト6)であること"
        );
    }

    #[test]
    fn test_token_at_utf16_supplementary() {
        use crate::types::{CachedLinderaToken, TokenStatus};
        // サロゲートペア文字「𠮷」(U+20BB7, 4バイト/2 UTF-16単位)を含む行で、
        // Position.character(UTF-16コード単位)が正しくバイト位置へ変換されること。
        // Linderaの補助面文字の分割挙動に依存しないよう、トークンは手組みで与える。
        let mut texts = lines("𠮷田中");
        texts[0].tokens = vec![CachedLinderaToken {
            details: vec!["名詞".to_string()],
            byte_start: 4, // "田中" (𠮷=4バイトの直後)
            byte_end: 10,
            tag: TokenStatus::Normal,
        }];
        let mut noop = |_line: &mut LineData| {};

        // UTF-16オフセット0 = 𠮷の手前 → トークン範囲外
        assert!(token_at(&mut texts, 0, 0, &mut noop).is_none());

        // UTF-16オフセット2 = 𠮷(2u16)の直後 → バイト4 → "田中"にヒット
        let (_, tkn) = token_at(&mut texts, 0, 2, &mut noop).expect("offset=2 should hit");
        assert_eq!(&texts[0].text[tkn.byte_start..tkn.byte_end], "田中");

        // UTF-16オフセット3 = 𠮷(2)+田(1) → バイト7("田中"内部) → ヒット。
        // 旧char単位解釈だと chars().take(3) = 𠮷+田+中 = バイト10 となり範囲外で
        // ミスしていた、UTF-16化の差分を検証するケース。
        let (_, tkn) = token_at(&mut texts, 0, 3, &mut noop).expect("offset=3 should hit");
        assert_eq!(&texts[0].text[tkn.byte_start..tkn.byte_end], "田中");
    }

    // ---- decorate_candidate のテスト ----

    #[test]
    fn test_decorate_before_closing_bracket_after_touten_no_period() {
        // 回帰: 「そうか、」の直後で補完すると「。わかった」の「。」が
        // 前文の読点と重なって「、。」になっていた。
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(CursorContext::BeforeClosingBracket, "わかった", Some('、'));
        assert_eq!(got, "わかった", "読点の直後には句点を前置しないこと");
    }

    #[test]
    fn test_decorate_before_closing_bracket_strips_trailing_period_even_with_touten_prev() {
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(
            CursorContext::BeforeClosingBracket,
            "わかった。",
            Some('、'),
        );
        assert_eq!(got, "わかった");
    }

    #[test]
    fn test_decorate_before_closing_bracket_strips_trailing_period_normally() {
        // 以前の排他分岐では「先頭に。が無い」branchに入り、末尾の「。」が
        // 除去されず「。わかった。」のような二重句点になっていた。
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(
            CursorContext::BeforeClosingBracket,
            "わかった。",
            Some('田'),
        );
        assert_eq!(
            got, "。わかった",
            "通常文字の後は前置しつつ、末尾の句点は除去すること"
        );
    }

    #[test]
    fn test_decorate_before_closing_bracket_prepends_period_after_normal_char() {
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(CursorContext::BeforeClosingBracket, "わかった", Some('田'));
        assert_eq!(got, "。わかった");
    }

    #[test]
    fn test_decorate_before_closing_bracket_no_prev_tail_prepends_period() {
        // 前文が空(行頭など)の場合は従来どおり前置する。
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(CursorContext::BeforeClosingBracket, "わかった", None);
        assert_eq!(got, "。わかった");
    }

    #[test]
    fn test_decorate_before_closing_bracket_candidate_already_starts_with_period() {
        use crate::cursor_context::decorate_candidate;
        let got = decorate_candidate(
            CursorContext::BeforeClosingBracket,
            "。わかった",
            Some('田'),
        );
        assert_eq!(got, "。わかった", "既に句点始まりなら重ねて前置しないこと");
    }

    #[test]
    fn test_decorate_empty_bracket_strips_trailing_period() {
        use crate::cursor_context::decorate_candidate;
        assert_eq!(
            decorate_candidate(CursorContext::EmptyBracket, "そうか。", Some('「')),
            "そうか"
        );
        assert_eq!(
            decorate_candidate(CursorContext::EmptyBracket, "そうか", Some('「')),
            "そうか"
        );
    }

    #[test]
    fn test_decorate_after_closing_bracket_prepends_newline() {
        use crate::cursor_context::decorate_candidate;
        assert_eq!(
            decorate_candidate(CursorContext::AfterClosingBracket, "地の文", Some('」')),
            "\n地の文"
        );
    }

    #[test]
    fn test_decorate_other_appends_period_unless_present() {
        use crate::cursor_context::decorate_candidate;
        assert_eq!(
            decorate_candidate(CursorContext::Other, "続きの文", Some('。')),
            "続きの文。"
        );
        assert_eq!(
            decorate_candidate(CursorContext::Other, "続きの文。", Some('。')),
            "続きの文。"
        );
    }

    // ---- extract_candidate_lines のテスト ----

    #[test]
    fn test_extract_candidate_lines_drops_preamble_before_last_blank_line() {
        // 回帰: モデルが意図説明2行→空行→実際の候補3行、という応答を返し、
        // 意図説明が候補として混入していた(DB実データで再現した件)。
        use crate::cursor_context::extract_candidate_lines;
        let response = "目前の平穏と、いつ何時始まるかの懸念をつなぐ一文。\n\
                         原少将の艦隊が置かれた状況を掘り下げ、次なる展開への導入とする。\n\
                         \n\
                         海図台に向き直り、緊張した空気が張り詰めていた。\n\
                         はるか水平線の彼方に不審な影を認めなかった。\n\
                         いつ敵情が現れても即座に対応できるよう命じた。";
        let got = extract_candidate_lines(response);
        assert_eq!(
            got,
            vec![
                "海図台に向き直り、緊張した空気が張り詰めていた。",
                "はるか水平線の彼方に不審な影を認めなかった。",
                "いつ敵情が現れても即座に対応できるよう命じた。",
            ]
        );
    }

    #[test]
    fn test_extract_candidate_lines_no_blank_line_keeps_all() {
        use crate::cursor_context::extract_candidate_lines;
        let response = "候補1\n候補2\n候補3";
        assert_eq!(
            extract_candidate_lines(response),
            vec!["候補1", "候補2", "候補3"]
        );
    }

    #[test]
    fn test_extract_candidate_lines_trailing_blank_line_does_not_discard_all() {
        // 末尾に空行があるだけの場合(最後の空行の後ろに実質行が無い)は
        // 前置き扱いせず、全体から空行だけを除いたものを返す。
        use crate::cursor_context::extract_candidate_lines;
        let response = "候補1\n候補2\n候補3\n\n";
        assert_eq!(
            extract_candidate_lines(response),
            vec!["候補1", "候補2", "候補3"]
        );
    }

    #[test]
    fn test_extract_candidate_lines_filters_blank_lines_without_preamble() {
        use crate::cursor_context::extract_candidate_lines;
        let response = "候補1\n\n候補2\n候補3";
        // 空行はあるが、後ろに実質行がある場合はその空行以降のみを採用する仕様どおり、
        // 空行より前の"候補1"は前置き扱いで捨てられる(観測された振る舞いに合わせた仕様)。
        assert_eq!(extract_candidate_lines(response), vec!["候補2", "候補3"]);
    }

    // ---- sentence_range_at のテスト ----

    fn sentence_range(
        td: &TestData,
        texts: &mut Vec<LineData>,
        line_no: usize,
        offset: usize,
    ) -> Range {
        crate::cursor_context::sentence_range_at(texts.as_mut_slice(), line_no, offset, |line| {
            td.tokenize(line)
        })
    }

    #[test]
    fn test_sentence_range_single_sentence_whole_document() {
        let mut text = lines("これは文章。");
        let td = TestData::new();
        let offset = "これは".chars().count();
        let r = sentence_range(&td, &mut text, 0, offset);
        let full = text[0].text.chars().count() as u32;
        assert_eq!(r, Range::new(Position::new(0, 0), Position::new(0, full)));
    }

    #[test]
    fn test_sentence_range_second_of_two_sentences() {
        let mut text = lines("一文目です。二文目です。");
        let td = TestData::new();
        let offset = "一文目です。二文".chars().count();
        let r = sentence_range(&td, &mut text, 0, offset);
        let start = "一文目です。".chars().count() as u32;
        let end = text[0].text.chars().count() as u32;
        assert_eq!(
            r,
            Range::new(Position::new(0, start), Position::new(0, end))
        );
    }

    #[test]
    fn test_sentence_range_cursor_on_trailing_period_included() {
        // カーソルが句点自体の上にあっても、その文の末尾として扱われる。
        let mut text = lines("一文目です。二文目です。");
        let td = TestData::new();
        let offset = "一文目です".chars().count(); // 「。」の直前(トークン境界)
        let r = sentence_range(&td, &mut text, 0, offset);
        let end = "一文目です。".chars().count() as u32;
        assert_eq!(r, Range::new(Position::new(0, 0), Position::new(0, end)));
    }

    #[test]
    fn test_sentence_range_dialogue_lines_crossing_lines() {
        // 1行目・2行目とも「」で閉じる独立した文。カーソルは2行目の台詞内。
        let mut text = lines("「セリフ１」\n「セリフ２」");
        let td = TestData::new();
        let offset = "「セリフ２".chars().count();
        let r = sentence_range(&td, &mut text, 1, offset);
        let line0_len = text[0].text.chars().count() as u32;
        let line1_len = text[1].text.chars().count() as u32;
        assert_eq!(
            r,
            Range::new(Position::new(0, line0_len), Position::new(1, line1_len))
        );
    }

    #[test]
    fn test_sentence_range_no_trailing_period_extends_to_document_end() {
        // 末尾に句点が無い(執筆途中)場合は文書末尾までを対象とする。
        let mut text = lines("一文目です。書きかけの文");
        let td = TestData::new();
        let offset = text[0].text.chars().count();
        let r = sentence_range(&td, &mut text, 0, offset);
        let start = "一文目です。".chars().count() as u32;
        let end = text[0].text.chars().count() as u32;
        assert_eq!(
            r,
            Range::new(Position::new(0, start), Position::new(0, end))
        );
    }

    #[test]
    fn test_sentence_range_cursor_at_document_start() {
        let mut text = lines("これは文章。次の文章。");
        let td = TestData::new();
        let r = sentence_range(&td, &mut text, 0, 0);
        let end = "これは文章。".chars().count() as u32;
        assert_eq!(r, Range::new(Position::new(0, 0), Position::new(0, end)));
    }
}
