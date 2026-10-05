//! LLM による文章診断(意味判断が要るもの)の純粋関数部分。
//!
//! 決定的ルール(`rules`/`slop`)が拾えない、場面の壮大化・無生物の擬人化・直訳の比喩・
//! 段落の閉じ方の均一化などを LLM に指摘させる。LSP への配線・LLM 呼び出し・記録は
//! `backend.rs` が担い、ここは `Backend`/`Client` に依存しない(入力は `&[LineData]` と
//! キャッシュ、出力は `Finding` と送信用 batch)。
//!
//! # キャッシュ
//! 小説では1行 = 1段落なので、段落本文のハッシュをキーに「その段落への指摘(空なら問題なし)」を
//! 持つ。未診断の段落だけを LLM へ送るので、再診断は差分しか課金されない。段落を編集すると
//! ハッシュが変わり、位置のずれた古い指摘は自然に表示されなくなる。
//!
//! # 位置特定
//! LLM には本文からの一字一句そのままの抜粋(`quote`)を返させ、その段落の中で文字列検索して
//! 範囲にする。見つからない抜粋(捏造・言い換え)と、台詞の中の抜粋は捨てる。

use std::collections::HashMap;

use super::{Finding, FindingRange, RuleId, Severity};
use crate::types::LineData;
use tracing::instrument;

/// 診断対象の1段落(= 1行)。
#[derive(Debug, Clone)]
pub(crate) struct Paragraph {
    pub line: usize,
    pub hash: u64,
    pub text: String,
}

/// LLM が返した1件の指摘(位置特定前)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawLlmFinding {
    pub quote: String,
    pub category: String,
    pub message: String,
}

/// `parse_review` の結果。`paragraph` は batch 内で振った `[P<line>]` の行番号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedFinding {
    pub paragraph: usize,
    pub raw: RawLlmFinding,
}

/// 段落ハッシュ → その段落への指摘(空 = 診断済みで問題なし)。URI ごとに持つ。
pub(crate) type ReviewCache = HashMap<u64, Vec<RawLlmFinding>>;

/// LLM へ1回で送る単位。
#[derive(Debug, Clone)]
pub(crate) struct Batch {
    /// `[P<line>] 本文` を改行で並べたもの(プロンプトの `{{TARGET}}`)。
    pub target: String,
    /// 直前の数段落(診断不要の文脈。プロンプトの `{{CONTEXT}}`)。
    pub context: String,
    /// この batch に含まれる段落(送信時点の行番号・ハッシュ・本文。応答待ち中に編集されても、
    /// 指摘の位置特定とキャッシュ登録は送信時点の本文で行う)。
    pub paragraphs: Vec<Paragraph>,
}

/// 直前の何段落を文脈として添えるか。
const CONTEXT_PARAGRAPHS: usize = 2;

pub(crate) fn hash_text(text: &str) -> u64 {
    farmhash::hash64(text.as_bytes())
}

/// 台詞(「」『』の内側)のバイト範囲を返す。`」` が行内で閉じない場合は行末まで。
#[instrument(ret)]
fn dialogue_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '「' | '『' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '」' | '』' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    ranges.push((start, i + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    if depth > 0 {
        ranges.push((start, text.len()));
    }
    ranges
}

fn in_dialogue(ranges: &[(usize, usize)], byte: usize) -> bool {
    ranges.iter().any(|&(a, b)| a <= byte && byte < b)
}

/// 台詞を除いた地の文に、文字(かな・漢字・英数)が1つでもあるか。
#[instrument(ret)]
fn has_narration(text: &str) -> bool {
    let dialogue = dialogue_ranges(text);
    text.char_indices()
        .any(|(i, c)| c.is_alphanumeric() && !in_dialogue(&dialogue, i))
}

/// 診断対象の段落を行順に集める。空行・見出し(`#`)・台詞だけの行は対象外。
#[instrument(skip(lines), ret)]
pub(crate) fn paragraphs(lines: &[LineData]) -> Vec<Paragraph> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(line, l)| {
            let text = l.text.trim();
            if text.is_empty() || text.starts_with('#') || !has_narration(text) {
                return None;
            }
            Some(Paragraph {
                line,
                hash: hash_text(&l.text),
                text: l.text.clone(),
            })
        })
        .collect()
}

/// 未診断の段落を `max_chars` を超えないよう batch に束ねる。同じ本文の段落は1回だけ送る。
/// 1段落だけで `max_chars` を超える場合は、その段落だけで1 batch にする。
#[instrument(skip(cache), ret)]
pub(crate) fn build_batches(
    paragraphs: &[Paragraph],
    cache: &ReviewCache,
    max_chars: usize,
) -> Vec<Batch> {
    let mut seen = std::collections::HashSet::new();
    let pending: Vec<usize> = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, p)| !cache.contains_key(&p.hash) && seen.insert(p.hash))
        .map(|(ix, _)| ix)
        .collect();

    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut used = 0usize;
    for ix in pending {
        let len = paragraphs[ix].text.chars().count();
        match groups.last_mut() {
            Some(g) if used + len <= max_chars => {
                g.push(ix);
                used += len;
            }
            _ => {
                groups.push(vec![ix]);
                used = len;
            }
        }
    }

    groups
        .into_iter()
        .map(|g| {
            let first = g[0];
            let ctx_from = first.saturating_sub(CONTEXT_PARAGRAPHS);
            let context = paragraphs[ctx_from..first]
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let target = g
                .iter()
                .map(|&ix| format!("[P{}] {}", paragraphs[ix].line, paragraphs[ix].text))
                .collect::<Vec<_>>()
                .join("\n");
            Batch {
                target,
                context,
                paragraphs: g.iter().map(|&ix| paragraphs[ix].clone()).collect(),
            }
        })
        .collect()
}

/// LLM の応答 JSON(`{"findings":[{paragraph,quote,category,message}]}`)を読む。
/// コードフェンスや前置きが付いていても、最初の `{` から最後の `}` までを試す。
/// 形が合わない要素は黙って捨てる(不正応答で診断全体を止めない)。
#[instrument(ret)]
pub(crate) fn parse_review(response: &str) -> Vec<ParsedFinding> {
    let value = serde_json::from_str::<serde_json::Value>(response.trim())
        .ok()
        .or_else(|| {
            let (a, b) = (response.find('{')?, response.rfind('}')?);
            serde_json::from_str(response.get(a..=b)?).ok()
        });
    let Some(items) = value
        .as_ref()
        .and_then(|v| v.get("findings"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let paragraph = it.get("paragraph")?.as_u64()? as usize;
            let quote = it.get("quote")?.as_str()?.trim().to_string();
            let category = it.get("category")?.as_str()?.trim().to_string();
            let message = it.get("message")?.as_str()?.trim().to_string();
            if quote.is_empty() || message.is_empty() {
                return None;
            }
            Some(ParsedFinding {
                paragraph,
                raw: RawLlmFinding {
                    quote,
                    category,
                    message,
                },
            })
        })
        .collect()
}

/// 段落本文 `text` の中で `quote` の最初の出現位置(バイト範囲)を返す。
/// 台詞の中から始まる出現は飛ばし、地の文にある最初の出現を採る。見つからなければ `None`。
#[instrument(ret)]
pub(crate) fn locate_quote(text: &str, quote: &str) -> Option<(usize, usize)> {
    if quote.is_empty() {
        return None;
    }
    let dialogue = dialogue_ranges(text);
    text.match_indices(quote)
        .map(|(start, _)| start)
        .find(|&start| !in_dialogue(&dialogue, start))
        .map(|start| (start, start + quote.len()))
}

/// 位置特定の結果に関わらず、1件を表示用メッセージにする(`[壮大化] …` の形)。
pub(crate) fn display_message(raw: &RawLlmFinding) -> String {
    if raw.category.is_empty() {
        raw.message.clone()
    } else {
        format!("[{}] {}", raw.category, raw.message)
    }
}

/// キャッシュを現在の本文へ当てはめ、表示できる指摘を `Finding` にする。
/// 段落が編集されてハッシュが変わっていれば、その段落の指摘は出ない。
#[instrument(ret)]
pub(crate) fn restore_findings(lines: &[LineData], cache: &ReviewCache) -> Vec<Finding> {
    let mut out = Vec::new();
    for (line_no, l) in lines.iter().enumerate() {
        if l.text.trim().is_empty() {
            continue;
        }
        let Some(raws) = cache.get(&hash_text(&l.text)) else {
            continue;
        };
        for raw in raws {
            let Some((start, end)) = locate_quote(&l.text, &raw.quote) else {
                continue;
            };
            out.push(Finding {
                rule: RuleId::LlmReview,
                severity: Severity::Information,
                message: display_message(raw),
                range: FindingRange {
                    start_line: line_no,
                    start_byte: start,
                    end_line: line_no,
                    end_byte: end,
                },
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn lines(text: &str) -> Vec<LineData> {
        text.lines()
            .map(|s| LineData::from_str(s).unwrap())
            .collect()
    }

    fn raw(quote: &str) -> RawLlmFinding {
        RawLlmFinding {
            quote: quote.to_string(),
            category: "壮大化".to_string(),
            message: "大げさ".to_string(),
        }
    }

    #[test]
    fn test_paragraphs_skips_blank_heading_and_dialogue_only() {
        let ps = paragraphs(&lines(
            "# 章\n\n地の文です。\n「台詞だけ」\n「台詞」と彼は言った。",
        ));
        let ls: Vec<usize> = ps.iter().map(|p| p.line).collect();
        assert_eq!(ls, vec![2, 4]);
    }

    #[test]
    fn test_parse_review_plain_and_fenced() {
        let json = r#"{"findings":[{"paragraph":3,"quote":"真実","category":"壮大化","message":"大げさ"}]}"#;
        let expected = vec![ParsedFinding {
            paragraph: 3,
            raw: raw("真実"),
        }];
        assert_eq!(parse_review(json), expected);
        assert_eq!(parse_review(&format!("```json\n{json}\n```")), expected);
    }

    #[test]
    fn test_parse_review_drops_malformed_items_and_garbage() {
        assert!(parse_review("not json").is_empty());
        assert!(
            parse_review(
                r#"{"findings":[{"paragraph":1,"quote":"","category":"a","message":"b"}]}"#
            )
            .is_empty()
        );
        assert!(
            parse_review(r#"{"findings":[{"quote":"x","category":"a","message":"b"}]}"#).is_empty()
        );
        assert!(parse_review(r#"{"findings":[]}"#).is_empty());
    }

    #[test]
    fn test_locate_quote_found_missing_and_dialogue() {
        // 最初の出現は台詞の中なので飛ばし、地の文の2つ目を採る
        let t = "彼は「真実だ」と言い、真実から目を背けた。";
        let (s, e) = locate_quote(t, "真実").unwrap();
        assert_eq!(&t[s..e], "真実");
        assert!(s > t.find('」').unwrap());
        // 台詞の中にしか無い抜粋は捨てる
        assert_eq!(locate_quote("彼は「真実だ」と言った。", "真実"), None);
        assert_eq!(
            locate_quote("真実を見た。", "真実"),
            Some((0, "真実".len()))
        );
        assert_eq!(locate_quote("真実を見た。", "捏造"), None);
    }

    #[test]
    fn test_restore_findings_hits_and_drops_stale_after_edit() {
        let original = "そこには残酷な真実があった。";
        let mut cache = ReviewCache::new();
        cache.insert(hash_text(original), vec![raw("残酷な真実")]);

        let f = restore_findings(&lines(original), &cache);
        assert_eq!(f.len(), 1, "{:?}", f);
        assert_eq!(f[0].rule, RuleId::LlmReview);
        assert!(f[0].message.starts_with("[壮大化]"));

        // 段落を編集するとハッシュが変わり、指摘は出なくなる
        assert!(restore_findings(&lines("そこには残酷な真実があったのだ。"), &cache).is_empty());
    }

    #[test]
    fn test_restore_findings_drops_unlocatable_quote() {
        let text = "静かな夜だった。";
        let mut cache = ReviewCache::new();
        cache.insert(hash_text(text), vec![raw("存在しない抜粋")]);
        assert!(restore_findings(&lines(text), &cache).is_empty());
    }

    #[test]
    fn test_build_batches_skips_cached_and_dedups_same_text() {
        let ls = lines("甲の段落です。\n乙の段落です。\n甲の段落です。");
        let ps = paragraphs(&ls);
        let mut cache = ReviewCache::new();
        cache.insert(ps[1].hash, vec![]); // 乙は診断済み(問題なし)
        let batches = build_batches(&ps, &cache, 1000);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paragraphs.len(), 1, "甲は1回だけ送る");
        assert_eq!(batches[0].paragraphs[0].line, 0);
        assert!(batches[0].target.starts_with("[P0] 甲"));
    }

    #[test]
    fn test_build_batches_respects_max_chars_and_oversized_paragraph() {
        let ls = lines("あいうえおかきくけこ。\nさしすせそたちつてと。\nなにぬねのはひふへほ。");
        let ps = paragraphs(&ls);
        // 1段落 11 文字。max 25 なら 2段落 + 1段落に分かれる
        let batches = build_batches(&ps, &ReviewCache::new(), 25);
        let sizes: Vec<usize> = batches.iter().map(|b| b.paragraphs.len()).collect();
        assert_eq!(sizes, vec![2, 1]);
        // max が1段落より小さくても、各段落は単独 batch として送られる
        let tiny = build_batches(&ps, &ReviewCache::new(), 5);
        assert_eq!(tiny.len(), 3);
    }

    #[test]
    fn test_build_batches_adds_preceding_context() {
        let ls = lines("一つ目の段落。\n二つ目の段落。\n三つ目の段落。");
        let ps = paragraphs(&ls);
        let mut cache = ReviewCache::new();
        cache.insert(ps[0].hash, vec![]);
        cache.insert(ps[1].hash, vec![]);
        let batches = build_batches(&ps, &cache, 1000);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].context.contains("一つ目") && batches[0].context.contains("二つ目"));
        assert!(!batches[0].target.contains("一つ目"));
    }
}
