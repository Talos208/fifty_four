//! 決定的ルール群。「1ルール = 1関数」で、判定は `details`(品詞情報)と表層形のみを見る。
//!
//! 共通方針: `名詞,固有名詞`(人名・組織名・地域名)は IPADIC が誤解析しやすく、キャラ名を
//! 撒き散らすだけで誤検知の温床になるため、内容語を扱うルールはすべて除外する
//! (`STok::is_proper_noun`)。

use super::sentence::{STok, SentenceSpan};
use super::{Finding, FindingRange, QualityConfig, RuleId, Severity, char_class_runs};
use crate::types::{LineData, TokenStatus};

/// `tokens[i]..=tokens[j]` を覆う `FindingRange` を作る。
fn token_range(tokens: &[STok], i: usize, j: usize) -> FindingRange {
    let a = &tokens[i];
    let b = &tokens[j];
    FindingRange {
        start_line: a.line,
        start_byte: a.byte_start,
        end_line: b.line,
        end_byte: b.byte_end,
    }
}

/// 昇順の出現位置列 `positions` を、クラスタ先頭からの距離が `window` 以内に収まるクラスタへまとめ、
/// 要素数が `threshold` 以上のクラスタだけを `(positions のインデックス開始, 終了)` で返す。
/// `WordRepeat`/`RedundantToIu` の「近接反復」判定で共用する。
fn cluster_matches(positions: &[usize], window: usize, threshold: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < positions.len() {
        let mut j = i;
        while j + 1 < positions.len() && positions[j + 1] - positions[i] <= window {
            j += 1;
        }
        if j - i + 1 >= threshold {
            out.push((i, j));
            i = j + 1; // 同じクラスタを重複報告しない
        } else {
            i += 1;
        }
    }
    out
}

// ---- 括弧・記号(生テキストベース) ----

/// ドキュメント末尾で括弧・ルビ記法が閉じ切れていないか。
/// `ensure_line_state` 畳み込み後の最終行 `state_after` を見るだけで判定できる。
pub(crate) fn bracket_mismatch(lines: &[LineData]) -> Vec<Finding> {
    let Some(last) = lines.last() else {
        return Vec::new();
    };
    if !last.state_after.is_resolved() || matches!(last.state_after, TokenStatus::Normal) {
        return Vec::new();
    }
    let end_line = lines.len() - 1;
    let end_byte = last.text.len();
    vec![Finding {
        rule: RuleId::BracketMismatch,
        severity: Severity::Warning,
        message: "括弧またはルビの記法(｜《》)が閉じられないまま文書が終わっています".to_string(),
        range: FindingRange {
            start_line: end_line,
            // 末尾1文字の先頭(マルチバイト文字の途中を指さないよう char 単位で求める)
            start_byte: last.text.char_indices().next_back().map_or(0, |(i, _)| i),
            end_line,
            end_byte,
        },
    }]
}

/// 三点リーダ「…」・ダッシュ「―」の連続数。出版慣行ではどちらも2個1組。
/// lindera はこれらの記号を未知語として一切トークン化しないため、行の生テキストを直接見る。
pub(crate) fn ellipsis_pair(lines: &[LineData], config: &QualityConfig) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_no, line) in lines.iter().enumerate() {
        for (target, label) in [('…', "三点リーダ「…」"), ('―', "ダッシュ「―」")] {
            for (start, end, count) in char_class_runs(&line.text, |c| c == target) {
                let ok = if config.ellipsis_strict {
                    count == 2
                } else {
                    count % 2 == 0
                };
                if ok {
                    continue;
                }
                findings.push(Finding {
                    rule: RuleId::EllipsisPair,
                    severity: Severity::Warning,
                    message: format!("{label}が{count}個連続しています(出版慣行では2個1組)"),
                    range: FindingRange {
                        start_line: line_no,
                        start_byte: start,
                        end_line: line_no,
                        end_byte: end,
                    },
                });
            }
        }
    }
    findings
}

// ---- 文単位のルール ----

/// 文末の助動詞(た/だ/です/ます/である 等)が閾値回数以上連続していないか。
///
/// 台詞(`in_dialogue`)を挟んだ場合や、空行を挟んだ段落区切りを挟んだ場合は、
/// その時点で連続をリセットする(台詞や改段落を挟んでも「地の文だけを抜き出せば連続」
/// という判定は、実際に読んだときの印象と合わないため)。
pub(crate) fn sentence_end_repeat(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    lines: &[LineData],
    config: &QualityConfig,
) -> Vec<Finding> {
    let threshold = config.sentence_end_repeat_threshold;
    if threshold < 2 {
        return Vec::new();
    }

    let mut findings = Vec::new();
    let mut streak_ixs: Vec<usize> = Vec::new();
    let mut streak_base: Option<&str> = None;
    let mut prev_end_line: Option<usize> = None;

    for (ix, s) in sentences.iter().enumerate() {
        if s.range.is_empty() {
            continue;
        }
        if s.in_dialogue {
            // 台詞を挟んだら連続は途切れる。
            flush_sentence_end_streak(&mut streak_ixs, &mut streak_base, tokens, sentences, threshold, &mut findings);
            prev_end_line = Some(tokens[s.range.end - 1].line);
            continue;
        }

        // 直前の文とのあいだに空行(段落区切り)があれば連続をリセットする。
        if let Some(prev_line) = prev_end_line {
            let cur_line = tokens[s.range.start].line;
            let paragraph_break = (prev_line + 1..cur_line)
                .any(|l| lines.get(l).map(|ln| ln.text.trim().is_empty()).unwrap_or(false));
            if paragraph_break {
                flush_sentence_end_streak(&mut streak_ixs, &mut streak_base, tokens, sentences, threshold, &mut findings);
            }
        }

        let ending = sentence_ending_auxiliary(tokens, s);
        match (streak_base, ending) {
            (Some(b), Some(e)) if b == e => {
                streak_ixs.push(ix);
            }
            _ => {
                flush_sentence_end_streak(&mut streak_ixs, &mut streak_base, tokens, sentences, threshold, &mut findings);
                if let Some(e) = ending {
                    streak_ixs.push(ix);
                    streak_base = Some(e);
                }
            }
        }
        prev_end_line = Some(tokens[s.range.end - 1].line);
    }
    flush_sentence_end_streak(&mut streak_ixs, &mut streak_base, tokens, sentences, threshold, &mut findings);

    findings
}

/// `sentence_end_repeat` の走査中に貯めた連続文末ストリークを、閾値を満たしていれば
/// `Finding` として確定させてクリアする。
fn flush_sentence_end_streak(
    streak_ixs: &mut Vec<usize>,
    streak_base: &mut Option<&str>,
    tokens: &[STok],
    sentences: &[SentenceSpan],
    threshold: usize,
    findings: &mut Vec<Finding>,
) {
    if streak_ixs.len() >= threshold {
        let base = streak_base.expect("streak_ixs が非空なら streak_base も Some のはず");
        let first = &sentences[streak_ixs[0]];
        let last = &sentences[*streak_ixs.last().unwrap()];
        findings.push(Finding {
            rule: RuleId::SentenceEndRepeat,
            severity: Severity::Information,
            message: format!("文末「{base}」が{}文連続しています", streak_ixs.len()),
            range: token_range(tokens, first.range.start, last.range.end - 1),
        });
    }
    streak_ixs.clear();
    *streak_base = None;
}

/// 文末(句点直前)の内容語を除いた最後のトークンが助動詞なら、その原形を返す。
fn sentence_ending_auxiliary<'a>(tokens: &'a [STok], span: &SentenceSpan) -> Option<&'a str> {
    tokens[span.range.clone()]
        .iter()
        .rev()
        .find(|t| t.pos() != "記号")
        .filter(|t| t.pos() == "助動詞")
        .map(|t| t.base())
}

/// 「AのBのCのD」: 連体化の「の」が1文中に閾値回数以上出現していないか。
pub(crate) fn no_chain(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for s in sentences {
        if s.in_dialogue {
            continue;
        }
        let positions: Vec<usize> = s
            .range
            .clone()
            .filter(|&i| {
                let t = &tokens[i];
                t.pos() == "助詞" && t.sub1() == "連体化" && t.surface == "の"
            })
            .collect();
        if positions.len() >= config.no_chain_threshold {
            findings.push(Finding {
                rule: RuleId::NoChain,
                severity: Severity::Information,
                message: format!(
                    "「の」が1文中に{}回使われています",
                    positions.len()
                ),
                range: token_range(tokens, positions[0], *positions.last().unwrap()),
            });
        }
    }
    findings
}

/// 係助詞「は」が直前の格助詞・接続助詞と結合した複合形(には/とは/では/からは 等)かどうか。
/// 実測上 IPADIC はこれらを [格助詞 に/と/で/から 等][係助詞 は] の2トークンに分割する。
/// 「彼にはAだが、彼女はBだ」のような対比構文は1文中で「は」を何度使っても自然であり、
/// 単純に同じ主題を並べる(=重複)こととは性質が異なるため、`topic_duplicate` の対象から外す。
fn is_compound_wa(tokens: &[STok], i: usize) -> bool {
    i > 0 && tokens[i - 1].pos() == "助詞"
}

/// 係助詞「は」(複合形を除く)が1文中に閾値回数以上出現していないか(主題の重複)。
pub(crate) fn topic_duplicate(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for s in sentences {
        if s.in_dialogue {
            continue;
        }
        let positions: Vec<usize> = s
            .range
            .clone()
            .filter(|&i| {
                let t = &tokens[i];
                t.pos() == "助詞"
                    && t.sub1() == "係助詞"
                    && t.surface == "は"
                    && !is_compound_wa(tokens, i)
            })
            .collect();
        if positions.len() >= config.topic_duplicate_threshold {
            findings.push(Finding {
                rule: RuleId::TopicDuplicate,
                severity: Severity::Information,
                message: format!(
                    "主題を示す「は」が1文中に{}回使われています",
                    positions.len()
                ),
                range: token_range(tokens, positions[0], *positions.last().unwrap()),
            });
        }
    }
    findings
}

/// 「〜することができる」(動詞 + こと(非自立) + が/は/も + できる)は「〜できる」に言い換えられる。
/// 台詞は口語表現として正当なため対象外。
pub(crate) fn redundant_can_do(tokens: &[STok], sentences: &[SentenceSpan]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for s in sentences {
        if s.in_dialogue {
            continue;
        }
        for i in s.range.clone() {
            if i + 3 >= s.range.end {
                continue;
            }
            let (v, koto, p, dekiru) = (&tokens[i], &tokens[i + 1], &tokens[i + 2], &tokens[i + 3]);
            if v.pos() == "動詞"
                && koto.pos() == "名詞"
                && koto.sub1() == "非自立"
                && koto.base() == "こと"
                && p.pos() == "助詞"
                && matches!(p.surface.as_str(), "が" | "は" | "も")
                && dekiru.pos() == "動詞"
                && dekiru.base() == "できる"
            {
                findings.push(Finding {
                    rule: RuleId::RedundantCanDo,
                    severity: Severity::Information,
                    message: "「〜することができる」は「〜できる」に言い換えられます".to_string(),
                    range: token_range(tokens, i, i + 3),
                });
            }
        }
    }
    findings
}

/// 「という」(格助詞「と」+ 動詞「いう」)の近接反復。台詞中の使用は対象外。
pub(crate) fn redundant_to_iu(tokens: &[STok], config: &QualityConfig) -> Vec<Finding> {
    let positions: Vec<usize> = (1..tokens.len())
        .filter(|&i| {
            tokens[i].pos() == "動詞"
                && tokens[i].base() == "いう"
                && !tokens[i].in_dialogue()
                && tokens[i - 1].pos() == "助詞"
                && tokens[i - 1].surface == "と"
                && !tokens[i - 1].in_dialogue()
        })
        .collect();

    cluster_matches(&positions, config.to_iu_window, config.to_iu_threshold)
        .into_iter()
        .map(|(a, b)| {
            let count = b - a + 1;
            let start = positions[a].saturating_sub(1);
            let end = positions[b];
            Finding {
                rule: RuleId::RedundantToIu,
                severity: Severity::Information,
                message: format!("「という」が近い範囲で{count}回使われています"),
                range: token_range(tokens, start, end),
            }
        })
        .collect()
}

/// い抜き言葉。IPADIC は「てる/でる」を 動詞,非自立 の単一トークンとして辞書登録しており、
/// 正規形「ている/でいる」との違いがそのままトークン単位で表れる(実測確認済み)。
/// 台詞中の口語的な省略は正当な表現なので対象外にする。
pub(crate) fn idrop_verb(tokens: &[STok]) -> Vec<Finding> {
    tokens
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            if t.pos() == "動詞"
                && t.sub1() == "非自立"
                && !t.in_dialogue()
                && matches!(t.base(), "てる" | "でる")
            {
                Some(Finding {
                    rule: RuleId::IdropVerb,
                    severity: Severity::Warning,
                    message: "い抜き言葉の可能性があります(例:「てる」→「ている」)".to_string(),
                    range: token_range(tokens, i, i),
                })
            } else {
                None
            }
        })
        .collect()
}

/// ら抜き言葉。IPADIC は「見れる」のような ら抜き形を(「見られる」とは別の)単一の
/// 動詞基本形として登録しているため、品詞情報だけでは正規の一段動詞(例: 呆れる・枯れる)と
/// 区別できない。誤検知を避けるため、実務上よく指摘される語の一覧に限定した経験則。
const RA_DROP_BASES: &[&str] = &[
    "見れる",
    "食べれる",
    "来れる",
    "出れる",
    "起きれる",
    "寝れる",
    "着れる",
    "降りれる",
    "開けれる",
    "閉めれる",
    "決めれる",
    "忘れれる",
    "覚えれる",
    "考えれる",
    "教えれる",
    "答えれる",
    "変えれる",
    "伝えれる",
    "信じれる",
];

/// 台詞中のら抜きは口語表現として正当なため対象外。
pub(crate) fn ra_drop(tokens: &[STok]) -> Vec<Finding> {
    tokens
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            if t.pos() == "動詞" && !t.in_dialogue() && RA_DROP_BASES.contains(&t.base()) {
                Some(Finding {
                    rule: RuleId::RaDrop,
                    // 語彙リストによる経験則で確度が落ちるため Warning ではなく Information。
                    severity: Severity::Information,
                    message: format!("「{}」はら抜き言葉の可能性があります", t.base()),
                    range: token_range(tokens, i, i),
                })
            } else {
                None
            }
        })
        .collect()
}

/// 二重敬語。「〜になる」(動詞,未然形)の直後に「れる/られる」(動詞,接尾)が続く形
/// (例:「お読みになられる」)は、「になる」自体が既に敬語のため冗長。
/// 台詞はキャラクターの口調(あえての過剰敬語等)の可能性があるため対象外。
pub(crate) fn double_honorific(tokens: &[STok]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for i in 0..tokens.len().saturating_sub(1) {
        let (a, b) = (&tokens[i], &tokens[i + 1]);
        if a.pos() == "動詞"
            && a.base() == "なる"
            && a.conj_form() == "未然形"
            && !a.in_dialogue()
            && b.pos() == "動詞"
            && b.sub1() == "接尾"
            && !b.in_dialogue()
            && matches!(b.base(), "れる" | "られる")
        {
            findings.push(Finding {
                rule: RuleId::DoubleHonorific,
                severity: Severity::Warning,
                message: "二重敬語の可能性があります(例:「〜になられる」→「〜になる」)"
                    .to_string(),
                range: token_range(tokens, i, i + 1),
            });
        }
    }
    findings
}

/// 形式名詞・補助動詞の「漢字表記 → 対応する仮名表記の原形」の対応表。
/// `kanji_formal_noun` が「作者の個人的な文体か、単なる表記ゆれか」を区別するのに使う。
const FORMAL_KANJI_KANA_PAIRS: &[(&str, &str)] = &[
    ("事", "こと"),
    ("物", "もの"),
    ("時", "とき"),
    ("為", "ため"),
    ("下さる", "くださる"),
    ("頂く", "いただく"),
    ("戴く", "いただく"),
];

/// 形式名詞・補助動詞の漢字表記(事/物/時/下さる/頂く 等)は一般に仮名表記が好まれるが、
/// 作者が一貫してその漢字表記を使っているなら、それはもう「誤り」ではなく文体(個性)。
/// 文書内で対応する仮名表記が一度も使われていなければ指摘しない(=表記が首尾一貫している
/// 限りは黙り、漢字/仮名が混在している語だけを指摘する)。台詞は対象外。
pub(crate) fn kanji_formal_noun(tokens: &[STok]) -> Vec<Finding> {
    fn is_formal(t: &STok) -> bool {
        t.sub1() == "非自立" && matches!(t.pos(), "名詞" | "動詞") && !t.in_dialogue()
    }

    let mut findings = Vec::new();
    for &(kanji, kana) in FORMAL_KANJI_KANA_PAIRS {
        let kanji_ixs: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| is_formal(t) && t.base() == kanji)
            .map(|(i, _)| i)
            .collect();
        if kanji_ixs.is_empty() {
            continue;
        }
        let kana_also_used = tokens.iter().any(|t| is_formal(t) && t.base() == kana);
        if !kana_also_used {
            continue;
        }
        for i in kanji_ixs {
            findings.push(Finding {
                rule: RuleId::KanjiFormalNoun,
                severity: Severity::Information,
                message: format!(
                    "形式的な「{kanji}」が仮名表記「{kana}」と混在しています(表記を統一すると読みやすくなります)"
                ),
                range: token_range(tokens, i, i),
            });
        }
    }
    findings
}

/// 読点なしで一定文字数以上続く区間(地の文のみ)。
/// 閾値は `config.comma_span_max_chars` を下限としつつ、文書全体の平均+2σがそれを
/// 上回る場合はそちらを使う(句読点を控えめに打つ文体の作者を毎回誤検知させないため)。
pub(crate) fn comma_span(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    let spans = collect_comma_spans(tokens, sentences);
    if spans.is_empty() {
        return Vec::new();
    }
    let lens: Vec<f32> = spans.iter().map(|&(_, _, chars)| chars as f32).collect();
    let effective_max = super::relative_threshold(&lens, config.comma_span_max_chars, 5, 2.0);

    spans
        .into_iter()
        .filter(|&(_, _, chars)| chars > effective_max)
        .map(|(start, end, chars)| Finding {
            rule: RuleId::CommaSpan,
            severity: Severity::Hint,
            message: format!("読点なしで{chars}文字続いています"),
            range: token_range(tokens, start, end),
        })
        .collect()
}

/// `comma_span` の下請け: 文(地の文のみ)を読点で分割した区間を
/// `(開始トークン index, 終了トークン index, 文字数)` の列として集める。
fn collect_comma_spans(tokens: &[STok], sentences: &[SentenceSpan]) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    for s in sentences {
        if s.in_dialogue || s.range.is_empty() {
            continue;
        }
        let mut span_start = s.range.start;
        let mut chars = 0usize;
        for i in s.range.clone() {
            let t = &tokens[i];
            chars += t.surface.chars().count();
            let is_comma = t.pos() == "記号" && t.sub1() == "読点";
            let is_last = i == s.range.end - 1;
            if is_comma || is_last {
                spans.push((span_start, i, chars));
                span_start = i + 1;
                chars = 0;
            }
        }
    }
    spans
}

/// 内容語(名詞,一般 / 動詞,自立 / 形容詞,自立。固有名詞・台詞は除外)の近接反復。
pub(crate) fn word_repeat(tokens: &[STok], config: &QualityConfig) -> Vec<Finding> {
    fn is_content_word(t: &STok) -> bool {
        if t.is_proper_noun() || t.in_dialogue() {
            return false;
        }
        matches!(
            (t.pos(), t.sub1()),
            ("名詞", "一般") | ("動詞", "自立") | ("形容詞", "自立")
        )
    }

    // 出現順(≒文書順)を保つため、初出順を別配列で管理する。
    let mut order: Vec<&str> = Vec::new();
    let mut by_base: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, t) in tokens.iter().enumerate() {
        if !is_content_word(t) || t.surface.chars().count() < config.word_repeat_min_chars {
            continue;
        }
        let base = t.base();
        if !by_base.contains_key(base) {
            order.push(base);
        }
        by_base.entry(base).or_default().push(i);
    }

    let mut findings = Vec::new();
    for base in order {
        let positions = &by_base[base];
        for (a, b) in cluster_matches(positions, config.word_repeat_window, config.word_repeat_threshold) {
            let (i, j) = (positions[a], positions[b]);
            findings.push(Finding {
                rule: RuleId::WordRepeat,
                severity: Severity::Information,
                message: format!("「{base}」が近い範囲で{}回繰り返し使われています", b - a + 1),
                range: token_range(tokens, i, j),
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{BracketColoring, Highlighter};
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

    fn build(text: &str) -> (Vec<STok>, Vec<SentenceSpan>) {
        let lines = fold(text);
        let tokens = super::super::sentence::build_tokens(&lines);
        let sentences = super::super::sentence::split_sentences(&tokens);
        (tokens, sentences)
    }

    /// `sentence_end_repeat` は段落区切り(空行)判定に `lines` も必要とするため、
    /// `lines`/`tokens`/`sentences` をまとめて返す。
    fn build_all(text: &str) -> (Vec<LineData>, Vec<STok>, Vec<SentenceSpan>) {
        let lines = fold(text);
        let tokens = super::super::sentence::build_tokens(&lines);
        let sentences = super::super::sentence::split_sentences(&tokens);
        (lines, tokens, sentences)
    }

    // ---- bracket_mismatch ----

    #[test]
    fn test_bracket_mismatch_detects_unclosed() {
        let lines = fold("「閉じられない台詞");
        let findings = bracket_mismatch(&lines);
        assert_eq!(findings.len(), 1, "{:?}", findings);
        assert_eq!(findings[0].rule, RuleId::BracketMismatch);
    }

    #[test]
    fn test_bracket_mismatch_ok_when_closed() {
        let lines = fold("「ちゃんと閉じた」台詞。");
        let findings = bracket_mismatch(&lines);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- ellipsis_pair ----

    #[test]
    fn test_ellipsis_pair_flags_single_dot() {
        let lines = fold("え…と思った。");
        let findings = ellipsis_pair(&lines, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
        assert_eq!(findings[0].rule, RuleId::EllipsisPair);
    }

    #[test]
    fn test_ellipsis_pair_allows_exactly_two() {
        let lines = fold("え……と思った。");
        let findings = ellipsis_pair(&lines, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- sentence_end_repeat ----

    #[test]
    fn test_sentence_end_repeat_detects_three_consecutive() {
        let (lines, tokens, sentences) = build_all("彼は走った。彼は笑った。彼は黙った。");
        let findings = sentence_end_repeat(&tokens, &sentences, &lines, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
        assert_eq!(findings[0].rule, RuleId::SentenceEndRepeat);
    }

    #[test]
    fn test_sentence_end_repeat_resets_across_dialogue() {
        // 台詞を挟むと連続は途切れる。片側2文ずつ(閾値3未満)なので指摘なし。
        // (台詞を単に除外して残りを詰めて数えると4文連続に化けてしまうバグの回帰確認)
        let (lines, tokens, sentences) =
            build_all("彼は走った。彼は笑った。「待って」彼は黙った。彼は泣いた。");
        let findings = sentence_end_repeat(&tokens, &sentences, &lines, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_sentence_end_repeat_resets_across_paragraph_break() {
        // 空行(段落区切り)を挟むと連続は途切れる。片側2文ずつなので指摘なし。
        let (lines, tokens, sentences) =
            build_all("彼は走った。彼は笑った。\n\n彼は黙った。彼は泣いた。");
        let findings = sentence_end_repeat(&tokens, &sentences, &lines, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_sentence_end_repeat_continues_across_plain_line_break() {
        // 空行を挟まない単なる改行は連続を切らない。
        let (lines, tokens, sentences) =
            build_all("彼は走った。\n彼は笑った。\n彼は黙った。");
        let findings = sentence_end_repeat(&tokens, &sentences, &lines, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    // ---- no_chain ----

    #[test]
    fn test_no_chain_detects_triple_no() {
        let (tokens, sentences) = build("友達のお姉さんの部屋の窓から見える景色。");
        let findings = no_chain(&tokens, &sentences, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_no_chain_ok_with_single_no() {
        let (tokens, sentences) = build("私の部屋は静かだった。");
        let findings = no_chain(&tokens, &sentences, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- topic_duplicate ----

    #[test]
    fn test_topic_duplicate_detects_two_ha() {
        let (tokens, sentences) = build("彼はこれは私のものだと言った。");
        let findings = topic_duplicate(&tokens, &sentences, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_topic_duplicate_ok_with_single_ha() {
        let (tokens, sentences) = build("彼は静かに笑った。");
        let findings = topic_duplicate(&tokens, &sentences, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_topic_duplicate_ok_for_niha_contrast() {
        // 「Xには…、Yは…」の対比構文は「には」の「は」が格助詞「に」と結合した複合形であり、
        // 単純な主題の重複ではないため指摘しない。
        let (tokens, sentences) = build("彼にはお金がないが、彼女は裕福だ。");
        let findings = topic_duplicate(&tokens, &sentences, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_topic_duplicate_detects_two_bare_ha_even_with_compound_present() {
        // 複合形「には」の「は」を除外しても、裸の「は」が2回あれば従来どおり指摘する。
        let (tokens, sentences) = build("彼にはお金がないが、彼は困り、妹は心配した。");
        let findings = topic_duplicate(&tokens, &sentences, &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    // ---- redundant_can_do ----

    #[test]
    fn test_redundant_can_do_detects_pattern() {
        let (tokens, sentences) = build("調べることができる。");
        let findings = redundant_can_do(&tokens, &sentences);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_redundant_can_do_ok_for_unrelated_koto_wo() {
        // 「出来ることをやった」は「こと」が目的語になる別構文で、できる止めではない。
        let (tokens, sentences) = build("出来ることをやった。");
        let findings = redundant_can_do(&tokens, &sentences);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_redundant_can_do_ignored_in_dialogue() {
        let (tokens, sentences) = build("「調べることができる」と彼は言った。");
        let findings = redundant_can_do(&tokens, &sentences);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- redundant_to_iu ----

    #[test]
    fn test_redundant_to_iu_detects_repeated_use() {
        let (tokens, _) = build(
            "彼はそういうという。彼女もそういうという。彼らもまたそういうという。",
        );
        let config = QualityConfig {
            to_iu_threshold: 3,
            ..Default::default()
        };
        let findings = redundant_to_iu(&tokens, &config);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_redundant_to_iu_ok_for_single_use() {
        let (tokens, _) = build("彼はこう思っているという。");
        let findings = redundant_to_iu(&tokens, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- idrop_verb ----

    #[test]
    fn test_idrop_verb_detects_teru() {
        let (tokens, _) = build("食べてる。");
        let findings = idrop_verb(&tokens);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_idrop_verb_ok_for_correct_form() {
        let (tokens, _) = build("食べている。");
        let findings = idrop_verb(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_idrop_verb_ignored_in_dialogue() {
        // 台詞内の口語的な省略(い抜き)は正当な表現なので指摘しない。
        let (tokens, _) = build("「食べてる」と彼は言った。");
        let findings = idrop_verb(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- ra_drop ----

    #[test]
    fn test_ra_drop_detects_mireru() {
        let (tokens, _) = build("見れる。");
        let findings = ra_drop(&tokens);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_ra_drop_ok_for_correct_form() {
        let (tokens, _) = build("見られる。");
        let findings = ra_drop(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_ra_drop_ignored_in_dialogue() {
        let (tokens, _) = build("「見れる」と彼は言った。");
        let findings = ra_drop(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- double_honorific ----

    #[test]
    fn test_double_honorific_detects_pattern() {
        let (tokens, _) = build("お読みになられる。");
        let findings = double_honorific(&tokens);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_double_honorific_ok_for_single_honorific() {
        let (tokens, _) = build("お読みになる。");
        let findings = double_honorific(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_double_honorific_ignored_in_dialogue() {
        let (tokens, _) = build("「お読みになられる」と彼は言った。");
        let findings = double_honorific(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- kanji_formal_noun ----

    #[test]
    fn test_kanji_formal_noun_detects_mixed_usage() {
        // 漢字表記「事」と仮名表記「こと」が同一文書内に混在している場合だけ指摘する。
        let (tokens, _) = build("本当の事を話した。調べることができる。");
        let findings = kanji_formal_noun(&tokens);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_kanji_formal_noun_ok_when_consistently_kanji() {
        // 仮名表記が一度も出てこなければ、作者の一貫した文体として指摘しない。
        let (tokens, _) = build("本当の事を話した。大切な事を伝えた。");
        let findings = kanji_formal_noun(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_kanji_formal_noun_ok_for_content_word() {
        // "物"が内容語(名詞,一般)として使われる場合は対象外。
        let (tokens, _) = build("それは物である。");
        let findings = kanji_formal_noun(&tokens);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- comma_span ----

    #[test]
    fn test_comma_span_detects_long_run() {
        let (tokens, sentences) = build(
            "彼はとても長い夜をひとりで過ごしながら何度も何度も同じ夢を見続けていたのだった。",
        );
        let config = QualityConfig {
            comma_span_max_chars: 20,
            ..Default::default()
        };
        let findings = comma_span(&tokens, &sentences, &config);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_comma_span_ok_with_commas() {
        let (tokens, sentences) = build("彼は、とても、静かに、笑った。");
        let config = QualityConfig {
            comma_span_max_chars: 10,
            ..Default::default()
        };
        let findings = comma_span(&tokens, &sentences, &config);
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- word_repeat ----

    #[test]
    fn test_word_repeat_detects_close_repetition() {
        let (tokens, _) = build("夜空を見た。夜空を見た。夜空を見た。");
        let config = QualityConfig {
            word_repeat_window: 40,
            word_repeat_threshold: 3,
            ..Default::default()
        };
        let findings = word_repeat(&tokens, &config);
        assert!(
            findings.iter().any(|f| f.message.contains("夜空")),
            "{:?}",
            findings
        );
    }

    #[test]
    fn test_word_repeat_excludes_proper_noun() {
        let (tokens, _) = build("田中は歩いた。田中は歩いた。田中は歩いた。");
        let config = QualityConfig {
            word_repeat_window: 40,
            word_repeat_threshold: 3,
            ..Default::default()
        };
        let findings = word_repeat(&tokens, &config);
        assert!(
            !findings.iter().any(|f| f.message.contains("田中")),
            "{:?}",
            findings
        );
    }
}
