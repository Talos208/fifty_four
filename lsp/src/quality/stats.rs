//! 文体統計プロファイル。文書全体を1パスして集計し、範囲に紐づけられるものだけを
//! 診断化する(`style_mixed`/`sentence_too_long`/`kana_run`/`kanji_run`)。
//! 名詞率・MVR・文長分散・会話文比率のように箇所を指せない集計値は `DocStats` として
//! 返すのみに留める(波線を引く場所がないものを診断にしても邪魔になるだけのため)。

use super::sentence::{STok, SentenceSpan};
use super::{Finding, FindingRange, QualityConfig, RuleId, Severity, char_class_runs};
use crate::types::LineData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Polite,
}

/// 文末(句点直前の最終内容トークン)から常体/敬体を推定する。
/// 助動詞「です/ます/ございます」→敬体、「だ/である/た」→常体、動詞・形容詞の基本形終止→常体。
/// いずれにも当てはまらない場合は判定不能として `None`(集計対象外)。
fn sentence_ending_style(tokens: &[STok], span: &SentenceSpan) -> Option<Style> {
    let t = tokens[span.range.clone()].iter().rev().find(|t| t.pos() != "記号")?;
    match (t.pos(), t.base()) {
        ("助動詞", "です") | ("助動詞", "ます") | ("助動詞", "ございます") => Some(Style::Polite),
        ("助動詞", "だ") | ("助動詞", "である") | ("助動詞", "た") => Some(Style::Plain),
        ("動詞", _) if t.conj_form() == "基本形" => Some(Style::Plain),
        ("形容詞", _) if t.conj_form() == "基本形" => Some(Style::Plain),
        _ => None,
    }
}

// 範囲に紐づかない集計値のため診断化はしない(`analyze_document` 参照)。現状は
// `run_quality_analysis` が `{:?}` でデバッグログに出すだけの利用だが、
// `#[derive(Debug)]` 経由の読み取りは dead_code 解析の対象外になるため、
// 将来レポート表示を足すまでの暫定として明示的に許可しておく。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct DocStats {
    /// 名詞率(要約的な文体ほど高い、樺島の品詞比率法)。
    pub noun_ratio: f32,
    pub verb_ratio: f32,
    /// MVR = (形容詞+副詞+連体詞+感動詞)/動詞 × 100。高いほど「ありさま描写的」。
    pub mvr: f32,
    pub kanji_ratio: f32,
    /// 会話文(括弧内)が占める文字数の割合。
    pub dialogue_char_ratio: f32,
    pub sentence_len_mean: f32,
    pub sentence_len_variance: f32,
    pub plain_sentence_count: usize,
    pub polite_sentence_count: usize,
}

pub(crate) fn compute_stats(tokens: &[STok], sentences: &[SentenceSpan]) -> DocStats {
    if tokens.is_empty() {
        return DocStats::default();
    }

    let mut pos_count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut dialogue_chars = 0usize;
    let mut total_chars = 0usize;
    let mut kanji_chars = 0usize;
    for t in tokens {
        *pos_count.entry(t.pos()).or_default() += 1;
        let n = t.surface.chars().count();
        total_chars += n;
        kanji_chars += t
            .surface
            .chars()
            .filter(|&c| crate::types::is_han_char(c))
            .count();
        if matches!(
            t.meaning,
            crate::types::TokenMeaning::Bracket
                | crate::types::TokenMeaning::InnerBracket
                | crate::types::TokenMeaning::BracketClose
        ) {
            dialogue_chars += n;
        }
    }

    let total = tokens.len() as f32;
    let get = |k: &str| *pos_count.get(k).unwrap_or(&0) as f32;
    let (noun, verb, adj, adv, adnominal, interjection) = (
        get("名詞"),
        get("動詞"),
        get("形容詞"),
        get("副詞"),
        get("連体詞"),
        get("感動詞"),
    );

    let lens: Vec<f32> = sentences
        .iter()
        .map(|s| {
            tokens[s.range.clone()]
                .iter()
                .map(|t| t.surface.chars().count())
                .sum::<usize>() as f32
        })
        .collect();
    let (mean, variance) = if lens.is_empty() {
        (0.0, 0.0)
    } else {
        let mean = lens.iter().sum::<f32>() / lens.len() as f32;
        let var = lens.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / lens.len() as f32;
        (mean, var)
    };

    let (plain, polite) = sentences.iter().filter(|s| !s.in_dialogue).fold(
        (0usize, 0usize),
        |(p, q), s| match sentence_ending_style(tokens, s) {
            Some(Style::Plain) => (p + 1, q),
            Some(Style::Polite) => (p, q + 1),
            None => (p, q),
        },
    );

    DocStats {
        noun_ratio: noun / total,
        verb_ratio: verb / total,
        mvr: if verb > 0.0 {
            (adj + adv + adnominal + interjection) / verb * 100.0
        } else {
            0.0
        },
        kanji_ratio: if total_chars > 0 {
            kanji_chars as f32 / total_chars as f32
        } else {
            0.0
        },
        dialogue_char_ratio: if total_chars > 0 {
            dialogue_chars as f32 / total_chars as f32
        } else {
            0.0
        },
        sentence_len_mean: mean,
        sentence_len_variance: variance,
        plain_sentence_count: plain,
        polite_sentence_count: polite,
    }
}

/// 常体/敬体混在。文書全体が一方に大きく偏っている(少数派比率 < `style_mixed_min_ratio`)
/// 場合だけ、少数派の文を個別に指摘する。台詞・判定不能文は対象外。
/// サンプルが少なすぎる(4文未満)場合は判定しない。
pub(crate) fn style_mixed(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    stats: &DocStats,
    config: &QualityConfig,
) -> Vec<Finding> {
    let polite = stats.polite_sentence_count;
    let plain = stats.plain_sentence_count;
    let total = polite + plain;
    if total < 4 {
        return Vec::new();
    }
    let (minority_style, minority_count) = if polite <= plain {
        (Style::Polite, polite)
    } else {
        (Style::Plain, plain)
    };
    if minority_count == 0 {
        return Vec::new();
    }
    let ratio = minority_count as f32 / total as f32;
    if ratio >= config.style_mixed_min_ratio {
        return Vec::new();
    }

    // 少数派の文の特定だけ再走査する(多数派/少数派の判定は集計済みの stats を使う)
    sentences
        .iter()
        .filter(|s| !s.in_dialogue && sentence_ending_style(tokens, s) == Some(minority_style))
        .map(|span| {
            let a = &tokens[span.range.start];
            let b = &tokens[span.range.end - 1];
            Finding {
                rule: RuleId::StyleMixed,
                severity: Severity::Information,
                message: "文書全体は常体/敬体のどちらかに統一されていますが、この文だけ異なります"
                    .to_string(),
                range: FindingRange {
                    start_line: a.line,
                    start_byte: a.byte_start,
                    end_line: b.line,
                    end_byte: b.byte_end,
                },
            }
        })
        .collect()
}

/// 1文の文字数が閾値を超えていないか。
pub(crate) fn sentence_too_long(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    sentences
        .iter()
        .filter(|s| !s.range.is_empty())
        .filter_map(|s| {
            let len: usize = tokens[s.range.clone()]
                .iter()
                .map(|t| t.surface.chars().count())
                .sum();
            if len > config.sentence_too_long_chars {
                let a = &tokens[s.range.start];
                let b = &tokens[s.range.end - 1];
                Some(Finding {
                    rule: RuleId::SentenceTooLong,
                    severity: Severity::Hint,
                    message: format!("1文が{len}文字あります(読点で区切ると読みやすくなります)"),
                    range: FindingRange {
                        start_line: a.line,
                        start_byte: a.byte_start,
                        end_line: b.line,
                        end_byte: b.byte_end,
                    },
                })
            } else {
                None
            }
        })
        .collect()
}

/// ひらがなの連続(「かな地獄」)。
pub(crate) fn kana_run(lines: &[LineData], config: &QualityConfig) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_no, line) in lines.iter().enumerate() {
        for (start, end, count) in char_class_runs(&line.text, crate::types::is_hiragana_char) {
            if count > config.kana_run_max {
                findings.push(Finding {
                    rule: RuleId::KanaRun,
                    severity: Severity::Hint,
                    message: format!("ひらがなが{count}文字連続しています"),
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

/// 漢字の連続。閾値は `config.kanji_run_max` を下限としつつ、文書全体の平均+2σがそれを
/// 上回る場合はそちらを使う(漢字を多用する硬めの文体の作者を毎回誤検知させないため)。
pub(crate) fn kanji_run(lines: &[LineData], config: &QualityConfig) -> Vec<Finding> {
    let runs: Vec<(usize, usize, usize, usize)> = lines
        .iter()
        .enumerate()
        .flat_map(|(line_no, line)| {
            char_class_runs(&line.text, crate::types::is_han_char)
                .into_iter()
                .map(move |(start, end, count)| (line_no, start, end, count))
        })
        .collect();
    if runs.is_empty() {
        return Vec::new();
    }
    let lens: Vec<f32> = runs.iter().map(|&(_, _, _, c)| c as f32).collect();
    let effective_max = super::relative_threshold(&lens, config.kanji_run_max, 8, 2.0);

    runs.into_iter()
        .filter(|&(_, _, _, count)| count > effective_max)
        .map(|(line_no, start, end, count)| Finding {
            rule: RuleId::KanjiRun,
            severity: Severity::Hint,
            message: format!("漢字が{count}文字連続しています"),
            range: FindingRange {
                start_line: line_no,
                start_byte: start,
                end_line: line_no,
                end_byte: end,
            },
        })
        .collect()
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

    fn build(text: &str) -> (Vec<STok>, Vec<SentenceSpan>) {
        let lines = fold(text);
        let tokens = super::super::sentence::build_tokens(&lines);
        let sentences = super::super::sentence::split_sentences(&tokens);
        (tokens, sentences)
    }

    // ---- compute_stats ----

    #[test]
    fn test_compute_stats_counts_dialogue_ratio() {
        let (tokens, sentences) = build("「静かだ」と彼は言った。");
        let stats = compute_stats(&tokens, &sentences);
        assert!(stats.dialogue_char_ratio > 0.0, "{:?}", stats);
        assert!(stats.dialogue_char_ratio < 1.0, "{:?}", stats);
    }

    #[test]
    fn test_compute_stats_empty_document() {
        let (tokens, sentences) = build("");
        let stats = compute_stats(&tokens, &sentences);
        assert_eq!(stats.noun_ratio, 0.0);
        assert_eq!(stats.sentence_len_mean, 0.0);
    }

    // ---- style_mixed ----

    #[test]
    fn test_style_mixed_flags_minority_sentence() {
        // 常体10文+敬体1文(比率1/11≈0.09 < 既定閾値0.1)で、少数派の敬体だけを指摘する。
        let (tokens, sentences) = build(
            "彼は歩いた。彼は走った。彼は笑った。彼は泣いた。彼は叫んだ。\
             彼は座った。彼は立った。彼は眠った。彼は起きた。彼は黙った。彼は喜びます。",
        );
        let findings = style_mixed(&tokens, &sentences, &compute_stats(&tokens, &sentences), &QualityConfig::default());
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_style_mixed_ok_when_uniform() {
        let (tokens, sentences) = build("彼は歩いた。彼は走った。彼は笑った。彼は泣いた。");
        let findings = style_mixed(&tokens, &sentences, &compute_stats(&tokens, &sentences), &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- sentence_too_long ----

    #[test]
    fn test_sentence_too_long_detects_long_sentence() {
        let (tokens, sentences) = build(
            "彼はとても長い夜をひとりで過ごしながら何度も何度も同じ夢を見続けていたのだった。",
        );
        let config = QualityConfig {
            sentence_too_long_chars: 20,
            ..Default::default()
        };
        let findings = sentence_too_long(&tokens, &sentences, &config);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_sentence_too_long_ok_for_short_sentence() {
        let (tokens, sentences) = build("彼は笑った。");
        let findings = sentence_too_long(&tokens, &sentences, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    // ---- kana_run / kanji_run ----

    #[test]
    fn test_kana_run_detects_long_run() {
        let lines = fold("ひらがなだけのぶんしょうがながくつづくとよみにくい");
        let config = QualityConfig {
            kana_run_max: 10,
            ..Default::default()
        };
        let findings = kana_run(&lines, &config);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_kana_run_ok_for_short_run() {
        let lines = fold("これはふつうの文章です");
        let findings = kana_run(&lines, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }

    #[test]
    fn test_kanji_run_detects_long_run() {
        let lines = fold("国立国会図書館所蔵資料調査報告書提出義務化推進委員会");
        let config = QualityConfig {
            kanji_run_max: 6,
            ..Default::default()
        };
        let findings = kanji_run(&lines, &config);
        assert_eq!(findings.len(), 1, "{:?}", findings);
    }

    #[test]
    fn test_kanji_run_ok_for_short_run() {
        let lines = fold("普通の文章です");
        let findings = kanji_run(&lines, &QualityConfig::default());
        assert!(findings.is_empty(), "{:?}", findings);
    }
}
