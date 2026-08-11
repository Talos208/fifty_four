//! テキストバッファ編集ユーティリティ。
//!
//! `main.rs` から切り出したモジュール。LSP の `didChange` 通知に含まれる差分
//! (`apply_changes`) と、補完・ログ出力で使う文字列整形ヘルパーをまとめる。

use crate::types::LineData;
use std::str::FromStr;
use tower_lsp_server::lsp_types::Range;
use tracing::instrument;

/// completion の候補に付ける `filter_text` を組み立てる。
/// クライアント(Zed)が「カーソル直前の語」を暗黙のフィルタクエリにする対策として、
/// カーソル手前の行テキスト全体を返す(`main.rs` の `completion` ハンドラ参照)。
/// どこを語境界と見なされても、そのクエリは必ずこの文字列の連続部分列になり一致する。
/// カーソル直前の「語」を返す。判定基準は Zed 本体の `CharClassifier::kind_with`
/// (crates/language/src/buffer.rs)と同一の `c.is_alphanumeric() || c == '_'` で、
/// Rust の `is_alphanumeric()` は Unicode の Letter/Number カテゴリを見るため
/// かな・漢字も語文字として連続列に含まれる(非語になるのは括弧・句読点・空白等)。
///
/// Zed はこの語を暗黙のフィルタクエリとして候補の label と照合するため、
/// LSP 補完の一般的な作法(トークンを置換し newText がトークンで始まる)に
/// 合わせて候補を組み立てる際、置換対象・接頭辞として使う。
/// FiftyFour 言語は `completion_query_characters`/`word_characters` を
/// 定義していないため、Zed 側もこの既定の英数字判定のみで語境界を決める。
#[instrument]
pub(crate) fn precursor_word(line_text: &str, utf16_offset: usize) -> &str {
    let end = crate::types::utf16_to_byte_offset(line_text, utf16_offset);
    let prefix = &line_text[..end];
    let start = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(end);
    &prefix[start..]
}

/// LLM 応答からコードフェンスや前後の説明文を除いた JSON 部分を取り出す。
///
/// 構造化出力(`ChatResponseFormat::JsonSpec`)非対応のモデルにプロンプトで JSON を
/// 要求すると、```json フェンスや「以下が結果です」といった前置きが付いてくることがある。
/// 最初の `{` から最後の `}` までを切り出すことでそれらを落とす。
#[instrument(skip(response))]
pub(crate) fn extract_json(response: &str) -> Option<&str> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    (start <= end).then(|| &response[start..=end])
}

#[instrument(skip(s))]
pub(crate) fn shorten(s: &str, len: usize) -> String {
    if s.chars().count() > len {
        s.chars().take(len - 2).collect::<String>() + "……"
    } else {
        s.to_owned()
    }
}

#[instrument(skip(s))]
pub(crate) fn shorten_middle(s: &str, len: usize) -> String {
    let c = &s.chars();
    let l = c.clone().count();
    if l > 25 && l > len {
        c.clone()
            .take(len - 12)
            .chain("……".chars())
            .chain(c.clone().skip(l - 10))
            .collect::<String>()
    } else {
        s.to_owned()
    }
}

#[instrument(skip(lines, text))]
pub(crate) fn apply_changes<T: AsRef<str>>(lines: &mut Vec<LineData>, text: T, range: Range) {
    let start_line = range.start.line as usize;
    let start_char = range.start.character as usize;
    let end_line = range.end.line as usize;
    let end_char = range.end.character as usize;

    // range.character は UTF-16 コード単位(positionEncoding=utf-16)。
    // utf16_to_byte_offset が行末クランプを内包する。
    let prefix = lines
        .get(start_line)
        .map(|l| {
            let ix = crate::types::utf16_to_byte_offset(&l.text, start_char);
            l.text[..ix].to_string()
        })
        .unwrap_or("".to_string());
    let suffix = lines
        .get(end_line)
        .map(|l| {
            let ix = crate::types::utf16_to_byte_offset(&l.text, end_char);
            l.text[ix..].to_string()
        })
        .unwrap_or("".to_string());

    let mut new_text = String::with_capacity(prefix.len() + text.as_ref().len() + suffix.len());
    new_text.push_str(&prefix);
    new_text.push_str(text.as_ref());
    new_text.push_str(&suffix);

    let cr = regex::Regex::new(r"(\r\n|\n)").unwrap();
    let new_lines: Vec<_> = cr
        .split(new_text.as_str())
        // .lines() // 文字列末尾に改行だけ挿入した場合、1行扱いになってしまう
        .map(|s| LineData::from_str(s).unwrap())
        .collect();

    let end = end_line.min(lines.len() - 1);
    lines.splice(start_line..=end, new_lines);

    // 括弧深さは前方の全行に依存する累積量。編集行以降のキャッシュを無効化する。
    // (新規行は from_str の時点で None。既存の後続行に残る陳腐化した Some をここで落とす。
    //  Option への None 代入だけなので、行数が多くてもコストは無視できる)
    let clear_from = start_line.min(lines.len());
    for l in lines[clear_from..].iter_mut() {
        l.bracket_depth_after = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use indoc::indoc;
    use tower_lsp_server::lsp_types::Position;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        }
    }

    fn lines(s: &str) -> Vec<LineData> {
        s.lines().map(|l| LineData::from_str(l).unwrap()).collect()
    }

    // ---- shorten_middle のテスト ----
    // code action のメニュー項目で使う。
    // 長い候補でも「文末がどう変わったか」が見えることが要件。

    #[test]
    fn test_shorten_middle_keeps_head_and_tail() {
        let s: String = (0..100)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let got = shorten_middle(&s, 60);
        assert_eq!(got.chars().count(), 60);
        // 先頭48文字 + "……" + 末尾10文字
        assert!(got.starts_with(&s.chars().take(48).collect::<String>()));
        assert!(got.ends_with(&s.chars().skip(90).collect::<String>()));
        assert!(got.contains("……"));
    }

    #[test]
    fn test_shorten_middle_returns_as_is_when_within_limit() {
        let s = "一文目です。二文目です。三文目です。";
        assert_eq!(shorten_middle(s, 60), s);
    }

    // ---- precursor_word のテスト ----
    // Rust の is_alphanumeric() はかな漢字も Unicode Letter として true を返すため
    // 語文字とみなされる(Zed の CharClassifier::kind_with も同じ判定基準)。
    // 括弧・句読点・空白のみが非語として境界になる。

    #[test]
    fn test_precursor_word_returns_word_run_up_to_cursor() {
        // 開き括弧「は非語なので、そこで区切られ「起きたら」だけが返る
        let out = precursor_word("「起きたら教えて", 5); // "「起きたら" まで(5 u16)
        assert_eq!(out, "起きたら");
    }

    #[test]
    fn test_precursor_word_stops_at_punctuation() {
        // 句点の直後はトークンが空
        let out = precursor_word("……した。", 5);
        assert_eq!(out, "");
    }

    #[test]
    fn test_precursor_word_offset_zero_is_empty() {
        assert_eq!(precursor_word("起きたら教えて", 0), "");
    }

    #[test]
    fn test_precursor_word_clamps_past_end() {
        let s = "起きた";
        assert_eq!(precursor_word(s, 100), s);
    }

    #[test]
    fn test_precursor_word_alphanumeric_and_underscore() {
        assert_eq!(precursor_word("foo_bar123", 10), "foo_bar123");
        // 直前が非語(スペース)ならそこで止まる
        assert_eq!(precursor_word("say foo_bar123", 14), "foo_bar123");
    }

    #[test]
    fn test_precursor_word_surrogate_pair() {
        // "a𠮷b": a=1u16, 𠮷=2u16(サロゲートペア), b=1u16。いずれも語文字扱い。
        let s = "a𠮷b";
        assert_eq!(precursor_word(s, 1), "a");
        assert_eq!(precursor_word(s, 3), "a𠮷"); // 𠮷 の直後
        // ペア中間(u16オフセット2)は utf16_to_byte_offset が文字の直後へ丸める
        assert_eq!(precursor_word(s, 2), "a𠮷");
    }

    // 1. 単一文字挿入
    #[test]
    fn test_insert_char() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(&mut ls, "!", range(0, 19, 0, 19));
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。!
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ))
        );
    }

    #[test]
    fn test_apply_changes_utf16_supplementary() {
        // "𠮷田" (𠮷=サロゲートペア,UTF-16で2単位/田=1単位)。
        // character=2 は 𠮷 の直後(UTF-16基準)を指す。
        // 旧char単位実装では chars().take(2) = "𠮷田" 全体が prefix になり
        // "!" が末尾(田の後ろ)に入ってしまっていた誤りを検証する。
        let mut ls = lines("𠮷田");
        apply_changes(&mut ls, "!", range(0, 2, 0, 2));
        assert_eq!(ls, lines("𠮷!田"));
    }

    // 2. 複数行の削除（text が空）
    #[test]
    fn test_delete_lines() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。
            驕れる人も久しからず、ただ春の夜の夢のごとし。
            猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
        ));
        apply_changes(&mut ls, "", range(1, 0, 3, 0));
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
            ))
        );
    }

    // 3. 1行を複数行に置換
    #[test]
    fn test_replace_line_with_multiline() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。
            驕れる人も久しからず、ただ春の夜の夢のごとし。"
        ));
        apply_changes(
            &mut ls,
            indoc!(
                "猛き者もつひにはほろびぬ、
                ひとへに風の前の塵に同じ。"
            ),
            range(1, 0, 1, 3),
        );
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                猛き者もつひにはほろびぬ、
                ひとへに風の前の塵に同じ。樹の花の色、盛者必衰の理をあらはす。
                驕れる人も久しからず、ただ春の夜の夢のごとし。"
            ))
        );
    }

    // 4. 行頭への挿入
    #[test]
    fn test_insert_at_line_start() {
        let mut ls = lines("諸行無常の響きあり。");
        apply_changes(&mut ls, "祇園精舍の鐘の声、", range(0, 0, 0, 0));
        assert_eq!(ls, lines("祇園精舍の鐘の声、諸行無常の響きあり。"));
    }

    // 5. 行末への挿入（改行を追加）
    #[test]
    fn test_insert_newline_at_end() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            娑羅双樹の花の色、盛者必衰の理をあらはす。"
        ));
        apply_changes(&mut ls, "\nただ春の夜の夢のごとし。", range(0, 19, 0, 19));
        assert_eq!(
            ls,
            lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                ただ春の夜の夢のごとし。
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ))
        );
    }

    // 6.tokenize後の行挿入
    #[test]
    fn test_insert_after_tokenize() {
        let mut ls = lines(indoc!(
            "祇園精舍の鐘の声、諸行無常の響きあり。
            「娑羅双樹」の花の色、「盛者必衰」の理をあらはす。"
        ));
        let hl = Highlighter::new();
        // 各行とも括弧は行内で閉じるので、深さ0起点の行単位 tokenize で足りる
        ls.iter_mut().for_each(|l| {
            hl.tokenize(l, &std::collections::HashSet::new());
        });

        assert_eq!(ls.len(), 2);
        // let mut s = String::new();
        // ls[1].tokens.iter().for_each(|t| {
        //     let _ = writeln!(s, "{:?}:\t{:?}", ls[1].surface(&t), t.details.split_at(4).0);
        // });
        /*
        "「":	["記号", "括弧開", "*", "*"]
        "娑":	["名詞", "一般", "*", "*"]
        "羅":	["名詞", "固有名詞", "人名", "姓"]
        "双":	["接頭詞", "名詞接続", "*", "*"]
        "樹":	["名詞", "一般", "*", "*"]
        "」":	["記号", "括弧閉", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "花":	["名詞", "一般", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "色":	["名詞", "一般", "*", "*"]
        "、":	["記号", "読点", "*", "*"]
        "「":	["記号", "括弧開", "*", "*"]
        "盛者":	["名詞", "一般", "*", "*"]
        "必":	["名詞", "一般", "*", "*"]
        "衰":	["名詞", "一般", "*", "*"]
        "」":	["記号", "括弧閉", "*", "*"]
        "の":	["助詞", "連体化", "*", "*"]
        "理":	["名詞", "一般", "*", "*"]
        "を":	["助詞", "格助詞", "一般", "*"]
        "あら":	["名詞", "一般", "*", "*"]
        "は":	["助詞", "係助詞", "*", "*"]
        "す":	["動詞", "自立", "*", "*"]
        "。":	["記号", "句点", "*", "*"]
        */
        assert_eq!(ls[1].tokens.len(), 23);

        // 一文字追加
        apply_changes(&mut ls, "\r\n", range(0, 19, 0, 19));

        assert_eq!(ls.len(), 3); // 改行分だけ行数が増える
        assert_eq!(ls[1].text, "");

        assert_ne!(ls[0].text.len(), 0); // 変更された行もtextはそのまま
        assert_eq!(ls[0].tokens.len(), 0); // 変更された行のtokenは空になる

        assert_eq!(ls[2].tokens.len(), 23); // 改行分後ろにずれるけどtokenはそのまま
    }
    /*
    size=2*/
    // 6. range が None → フルテキスト置換
    /*   #[test]
        fn test_full_replace_when_range_none() {
            let mut ls = lines(indoc!(
                "祇園精舍の鐘の声、諸行無常の響きあり。
                娑羅双樹の花の色、盛者必衰の理をあらはす。"
            ));
            apply_changes(
                &mut ls,
                indoc!(
                    "驕れる人も久しからず、ただ春の夜の夢のごとし。
                    猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
                ),
                &[change(
                    None,
                )],
            );
            assert_eq!(
                ls,
                lines(indoc!(
                    "驕れる人も久しからず、ただ春の夜の夢のごとし。
                    猛き者もつひにはほろびぬ、ひとへに風の前の塵に同じ。"
                ))
            );
        }
    // 7. 複数 change の順次適用
    #[test]
    fn test_multiple_changes_applied_sequentially() {
        let mut ls = lines("abc");
        apply_changes(
            &mut ls,
            &[
                change(Some(range(0, 3, 0, 3)), "d"),
                change(Some(range(0, 0, 0, 1)), "A"),
            ],
        );
        assert_eq!(ls, lines("Abcd"));
    }
    */
}
