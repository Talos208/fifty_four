//! 「AI が書いたような日本語」の決定的検出ルール(地の文のみ)。
//!
//! 参照元(いずれも技術記事・ブログ向けの項目が多く、小説に当てはまるものだけを採った):
//! - nanaism/yomiyasu `references/slop-catalog.md` / `domains/essay.md`
//! - coji/natural-japanese `references/forbidden-patterns.md` / `translationese.md` / `scripts/lint.py`
//! - iKora128/stop-ai-slop-jp `references/phrases.md` / `structures.md`
//! - k16shikano「日本語技術文書の文章規範」(LLM っぽい空句・翻訳調の比喩と擬人化)
//!
//! 台詞(`in_dialogue`)はキャラクターの口調として正当な場合が多いので、記号の誤用(`punct_char`)を
//! 除いて対象外にする。語彙リストは `QualityConfig` に載せてあり、`initialization_options` の
//! `quality.rules` から置き換え・追加できる(作風によって正当に頻出する語があるため)。

use std::ops::Range;

use super::rules::{cluster_matches, sentence_streak_findings, token_range};
use super::sentence::{STok, SentenceSpan};
use super::{Finding, FindingRange, QualityConfig, RuleId, Severity, char_class_runs};
use crate::types::LineData;
use tracing::instrument;

// ---- 既定の語彙リスト(出典はコメントに残す) ----

/// 壮大化熟語。出典: slop-catalog §4「必殺技造語」、stop-ai-slop-jp phrases「必殺技造語」。
/// 「結末」は筋の説明で普通に使うので除外した。
pub(crate) const DEFAULT_GRANDIOSE_WORDS: &[&str] = &[
    "真理",
    "真実",
    "宿命",
    "運命",
    "境地",
    "極致",
    "究極",
    "虚飾",
    "虚像",
    "美学",
    "深淵",
    "禁欲的",
    "優美",
    "冷徹",
    "冷酷",
    "残酷",
    "凝縮",
    "結晶",
    "結実",
    "重厚感",
    "枯淡",
];

/// 質感・認知・評価を装う疑似具体語。出典: slop-catalog §5、stop-ai-slop-jp phrases「AI偏愛語」。
/// 「手触り」「体温」「熱量」は小説では字義どおりに使うことが多いので除外した。
pub(crate) const DEFAULT_PSEUDO_CONCRETE_WORDS: &[&str] = &[
    "肌感",
    "肌感覚",
    "温度感",
    "解像度",
    "腹落ち",
    "メンタルモデル",
    "本質的",
    "言語化",
    "地に足のついた",
    "地に足の着いた",
    "等身大",
];

/// LLM 定型句。出典: natural-japanese forbidden-patterns(結論の押し付け/過剰な強調/正面から系)、
/// slop-catalog §6、k16shikano gist「LLM っぽい表現の禁止」。
pub(crate) const DEFAULT_STOCK_PHRASES: &[&str] = &[
    "重要なのは",
    "大切なのは",
    "言うまでもなく",
    "言うまでもありません",
    "と言えるだろう",
    "と言えるでしょう",
    "に他ならない",
    "に他なりません",
    "と言っても過言ではない",
    "と言っても過言ではありません",
    "興味深いことに",
    "驚くべきことに",
    "結論から言うと",
    "まとめると",
    "重要性を痛感",
    "重要性を再認識",
    "正面から扱",
    "ここで注目すべきは",
    "ここで重要なのは",
    "一概には言えな",
    "いかがでしたでしょうか",
];

/// 翻訳調の定型。出典: natural-japanese translationese.md。
/// 「することができる」は既存の `redundant-can-do` が担当する。
pub(crate) const DEFAULT_TRANSLATIONESE_PHRASES: &[&str] = &[
    "することによって",
    "という観点から",
    "という観点で",
    "という点で",
    "であることは間違いない",
    "ことが可能",
    "にとって不可欠",
    "にとって重要",
];

/// 「〜を持つ」の目的語になると翻訳調になる抽象名詞(have significance の直訳)。
/// 出典: natural-japanese translationese.md「〜を持つ」。「力」「魔力」等は小説で字義どおりなので除外。
pub(crate) const DEFAULT_ABSTRACT_HAVE_NOUNS: &[&str] =
    &["意味", "影響", "価値", "役割", "可能性", "重要性", "意義"];

/// 無生物主語(This fact suggests ... の直訳)になりやすい主語。
/// 出典: natural-japanese translationese.md「無生物主語 + 他動詞」、stop-ai-slop-jp structures §1-1。
pub(crate) const DEFAULT_INANIMATE_SUBJECTS: &[&str] =
    &["これ", "それ", "事実", "結果", "データ", "数字"];

/// 上記の主語に続く他動詞の語幹(活用形をまとめて拾うため語幹で持つ)。
/// 「示す」単体は「彼が示した」のように人が主語の文でも出るので入れていない。
pub(crate) const DEFAULT_INANIMATE_VERBS: &[&str] = &[
    "もたらし",
    "もたらす",
    "示唆",
    "意味し",
    "意味す",
    "浮き彫り",
    "生み出し",
    "生み出す",
    "反映",
    "証明",
    "物語っ",
    "物語る",
];

/// 根拠のない強調副詞。出典: stop-ai-slop-jp phrases「強度の振り切り」。
pub(crate) const DEFAULT_INTENSIFIERS: &[&str] = &[
    "非常に",
    "とても",
    "かなり",
    "本当に",
    "実に",
    "極めて",
    "すごく",
    "めちゃくちゃ",
];

/// ヘッジ(保険)表現の語幹。出典: stop-ai-slop-jp phrases「三段ヘッジ」。
pub(crate) const DEFAULT_HEDGES: &[&str] = &[
    "かもしれ",
    "可能性がある",
    "ことがある",
    "場合がある",
    "とは限ら",
    "と言えなくも",
];

/// 否定→肯定の対比を作る定型。出典: natural-japanese `detect_antithesis_repetition`、
/// stop-ai-slop-jp structures §2-1「二項対比」。
const ANTITHESIS_PHRASES: &[&str] = &["ではなく", "じゃなく", "だけでなく"];

/// 「Aではなかった。Bだった。」の前半(否定で終わる文)。
const ANTITHESIS_NEGATIVE_ENDINGS: &[&str] =
    &["ではない", "ではなかった", "じゃない", "じゃなかった"];

// ---- 共通ヘルパー ----

/// `tokens[range]` の表層形を連結した文字列から `phrases` を探し、一致を覆うトークン範囲
/// `(先頭トークン, 末尾トークン, 一致した語句)` の列を位置順で返す。
///
/// 形態素の切れ目に依存せず「という観点から」のような連なりを探すための部分一致。
/// 台詞のトークンを含む一致は捨てる。重なる一致は、先に始まるもの(同位置なら長いもの)だけを採る。
#[instrument(skip(tokens, phrases), ret)]
fn scan_phrases<'a, S: AsRef<str>>(
    tokens: &[STok],
    range: Range<usize>,
    phrases: &'a [S],
) -> Vec<(usize, usize, &'a str)> {
    if range.is_empty() {
        return Vec::new();
    }
    let mut joined = String::new();
    let mut starts = Vec::with_capacity(range.len());
    for t in &tokens[range.clone()] {
        starts.push(joined.len());
        joined.push_str(&t.surface);
    }
    // `joined` のバイト位置 -> それを含むトークンの index
    let token_at = |byte: usize| range.start + starts.partition_point(|&s| s <= byte) - 1;

    let mut hits: Vec<(usize, usize, &'a str)> = Vec::new();
    for p in phrases {
        let p = p.as_ref();
        if p.is_empty() {
            continue;
        }
        for (b, m) in joined.match_indices(p) {
            let (i, j) = (token_at(b), token_at(b + m.len() - 1));
            if tokens[i..=j].iter().any(STok::in_dialogue) {
                continue;
            }
            hits.push((i, j, p));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.len().cmp(&a.2.len())));

    let mut out = Vec::new();
    let mut last_end: Option<usize> = None;
    for (i, j, p) in hits {
        if last_end.is_some_and(|e| i <= e) {
            continue;
        }
        last_end = Some(j);
        out.push((i, j, p));
    }
    out
}

/// 文単位に `phrases` を探し、すべての一致を位置順で返す。
#[instrument(skip(tokens, phrases), ret)]
fn scan_sentences<'a, S: AsRef<str>>(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    phrases: &'a [S],
) -> Vec<(usize, usize, &'a str)> {
    sentences
        .iter()
        .flat_map(|s| scan_phrases(tokens, s.range.clone(), phrases))
        .collect()
}

/// 同じ `line` のトークンが並ぶ範囲(行 = 小説の1段落)ごとに分ける。
#[instrument(skip(tokens), ret)]
fn line_ranges(tokens: &[STok]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 1..=tokens.len() {
        if i == tokens.len() || tokens[i].line != tokens[start].line {
            out.push(start..i);
            start = i;
        }
    }
    if tokens.is_empty() {
        out.clear();
    }
    out
}

/// 記号を除いた文字数。
fn content_chars(tokens: &[STok], span: &SentenceSpan) -> usize {
    tokens[span.range.clone()]
        .iter()
        .filter(|t| t.pos() != "記号")
        .map(|t| t.surface.chars().count())
        .sum()
}

// ---- 語彙ルール ----

/// 壮大化熟語が近い範囲に集中していないか。小説では単発なら正当(「運命」「残酷」など)なので、
/// 密集だけを指摘する。
#[instrument(skip(tokens), ret)]
pub(crate) fn grandiose_word(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    let hits = scan_sentences(tokens, sentences, &config.grandiose_words);
    let positions: Vec<usize> = hits.iter().map(|h| h.0).collect();
    cluster_matches(
        &positions,
        config.grandiose_window,
        config.grandiose_threshold,
    )
    .into_iter()
    .map(|(a, b)| {
        let mut words: Vec<&str> = Vec::new();
        for h in &hits[a..=b] {
            if !words.contains(&h.2) {
                words.push(h.2);
            }
        }
        Finding {
            rule: RuleId::GrandioseWord,
            severity: Severity::Hint,
            message: format!(
                "大げさな熟語(「{}」)が近い範囲で{}回使われています",
                words.join("」「"),
                b - a + 1
            ),
            range: token_range(tokens, hits[a].0, hits[b].1),
        }
    })
    .collect()
}

/// 質感・認知を装う語(肌感・解像度・腹落ち など)。
#[instrument(skip(tokens), ret)]
pub(crate) fn pseudo_concrete(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    scan_sentences(tokens, sentences, &config.pseudo_concrete_words)
        .into_iter()
        .map(|(i, j, p)| Finding {
            rule: RuleId::PseudoConcrete,
            severity: Severity::Information,
            message: format!("「{p}」は具体性を装う語です(実際の描写に置き換えられませんか)"),
            range: token_range(tokens, i, j),
        })
        .collect()
}

/// LLM が好む定型句(重要なのは・と言えるだろう・に他ならない など)。
#[instrument(skip(tokens), ret)]
pub(crate) fn stock_phrase(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    scan_sentences(tokens, sentences, &config.stock_phrases)
        .into_iter()
        .map(|(i, j, p)| Finding {
            rule: RuleId::StockPhrase,
            severity: Severity::Information,
            message: format!(
                "「{p}」は生成文によく出る定型句です(削って事実から書き始められませんか)"
            ),
            range: token_range(tokens, i, j),
        })
        .collect()
}

/// 翻訳調(することによって・という観点から・抽象名詞+を持つ など)。
#[instrument(skip(tokens), ret)]
pub(crate) fn translationese(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    let mut findings: Vec<Finding> =
        scan_sentences(tokens, sentences, &config.translationese_phrases)
            .into_iter()
            .map(|(i, j, p)| Finding {
                rule: RuleId::Translationese,
                severity: Severity::Information,
                message: format!("「{p}」は英語直訳のような言い回しです"),
                range: token_range(tokens, i, j),
            })
            .collect();

    // 抽象名詞 + を + 持つ(「刀を持つ」のような具体物は対象外)
    for i in 0..tokens.len().saturating_sub(2) {
        let (n, o, v) = (&tokens[i], &tokens[i + 1], &tokens[i + 2]);
        if n.in_dialogue() || v.in_dialogue() {
            continue;
        }
        if config.abstract_have_nouns.contains(&n.surface)
            && o.pos() == "助詞"
            && o.surface == "を"
            && v.pos() == "動詞"
            && v.base() == "持つ"
        {
            findings.push(Finding {
                rule: RuleId::Translationese,
                severity: Severity::Information,
                message: format!(
                    "「{0}を持つ」は「{0}がある」のほうが自然です(have の直訳)",
                    n.surface
                ),
                range: token_range(tokens, i, i + 2),
            });
        }
    }
    findings.sort_by_key(|f| (f.range.start_line, f.range.start_byte));
    findings
}

/// 無生物主語 + 他動詞(「この事実は〜を示唆している」)。人や状況を主語に戻したほうが日本語らしい。
#[instrument(skip(tokens), ret)]
pub(crate) fn inanimate_subject(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    /// 主語から述語を探す最大トークン数(離れすぎた共起を拾わない)。
    const PREDICATE_REACH: usize = 30;

    let mut findings = Vec::new();
    for s in sentences {
        for i in s.range.clone() {
            let t = &tokens[i];
            if t.in_dialogue() || !config.inanimate_subjects.contains(&t.surface) {
                continue;
            }
            let Some(p) = tokens.get(i + 1).filter(|_| i + 1 < s.range.end) else {
                continue;
            };
            if !(p.pos() == "助詞" && matches!(p.surface.as_str(), "は" | "が")) {
                continue;
            }
            let end = (i + 2 + PREDICATE_REACH).min(s.range.end);
            if let Some(&(_, j, verb)) =
                scan_phrases(tokens, i + 2..end, &config.inanimate_verbs).first()
            {
                findings.push(Finding {
                    rule: RuleId::InanimateSubject,
                    severity: Severity::Information,
                    message: format!(
                        "「{}」が「{verb}…」の主語になっています(人や状況を主語にすると自然です)",
                        t.surface
                    ),
                    range: token_range(tokens, i, j),
                });
            }
        }
    }
    findings
}

// ---- 構造ルール ----

/// 「AではなくB」型の対比の反復。単発は自然な技法だが、同じ型が近い範囲で続くと型が目立つ。
/// 「ではなく」等に加えて、「Aではなかった。Bだった。」(否定で終わる文 + 短い肯定文)も1回と数える。
#[instrument(skip(tokens), ret)]
pub(crate) fn antithesis_repeat(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    config: &QualityConfig,
) -> Vec<Finding> {
    /// 否定文の直後に来る「短い肯定文」とみなす最大文字数。
    const AFFIRMATION_MAX_CHARS: usize = 20;

    let mut hits: Vec<(usize, usize)> = scan_sentences(tokens, sentences, ANTITHESIS_PHRASES)
        .into_iter()
        .map(|(i, j, _)| (i, j))
        .collect();

    for pair in sentences.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.range.is_empty() || b.range.is_empty() || a.in_dialogue || b.in_dialogue {
            continue;
        }
        let a_text: String = tokens[a.range.clone()]
            .iter()
            .filter(|t| t.pos() != "記号")
            .map(|t| t.surface.as_str())
            .collect();
        if ANTITHESIS_NEGATIVE_ENDINGS
            .iter()
            .any(|e| a_text.ends_with(e))
            && content_chars(tokens, b) <= AFFIRMATION_MAX_CHARS
        {
            hits.push((a.range.start, b.range.end - 1));
        }
    }
    hits.sort_unstable();
    hits.dedup_by_key(|h| h.0);

    let positions: Vec<usize> = hits.iter().map(|h| h.0).collect();
    cluster_matches(
        &positions,
        config.antithesis_window,
        config.antithesis_threshold,
    )
    .into_iter()
    .map(|(a, b)| Finding {
        rule: RuleId::AntithesisRepeat,
        severity: Severity::Information,
        message: format!(
            "「AではなくB」型の対比が近い範囲で{}回使われています(直接Bと書けませんか)",
            b - a + 1
        ),
        range: token_range(tokens, hits[a].0, hits[b].1),
    })
    .collect()
}

/// 文頭(最初の2形態素)が同じ文の連続(「彼は…。彼は…。彼は…。」)。
#[instrument(skip(tokens, lines), ret)]
pub(crate) fn sentence_start_repeat(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    lines: &[LineData],
    config: &QualityConfig,
) -> Vec<Finding> {
    sentence_streak_findings(
        tokens,
        sentences,
        lines,
        config.sentence_start_repeat_threshold,
        RuleId::SentenceStartRepeat,
        Severity::Information,
        |tokens, s| {
            let head: String = tokens[s.range.clone()]
                .iter()
                .filter(|t| t.pos() != "記号")
                .take(2)
                .map(|t| t.surface.as_str())
                .collect();
            (!head.is_empty()).then_some(head)
        },
        |base, n| format!("文頭「{base}」が{n}文連続しています"),
    )
}

/// 短い文の連発(「シンプル。それだけ。それが本質。」)。山場の演出なら正当なので最弱の Hint。
#[instrument(skip(tokens, lines), ret)]
pub(crate) fn fragment_run(
    tokens: &[STok],
    sentences: &[SentenceSpan],
    lines: &[LineData],
    config: &QualityConfig,
) -> Vec<Finding> {
    let max = config.fragment_max_chars;
    sentence_streak_findings(
        tokens,
        sentences,
        lines,
        config.fragment_run,
        RuleId::FragmentRun,
        Severity::Hint,
        |tokens, s| {
            let n = content_chars(tokens, s);
            (n > 0 && n <= max).then(|| "短文".to_string())
        },
        |_, n| {
            format!("{max}字以下の短い文が{n}文連続しています(決め台詞の連発になっていませんか)")
        },
    )
}

/// 根拠のない強調副詞(非常に・とても・本当に など)が1段落に集中していないか。
#[instrument(skip(tokens), ret)]
pub(crate) fn intensifier_density(tokens: &[STok], config: &QualityConfig) -> Vec<Finding> {
    per_line_density(
        tokens,
        &config.intensifiers,
        config.intensifier_per_line,
        RuleId::IntensifierDensity,
        |n| format!("強調の副詞が1段落に{n}回使われています(強い言葉は要所だけに)"),
    )
}

/// ヘッジ表現(かもしれない・可能性がある・ことがある など)の重ね掛け。
#[instrument(skip(tokens), ret)]
pub(crate) fn hedge_stack(tokens: &[STok], config: &QualityConfig) -> Vec<Finding> {
    per_line_density(
        tokens,
        &config.hedges,
        config.hedge_per_line,
        RuleId::HedgeStack,
        |n| format!("ヘッジ表現が1段落に{n}回重なっています(言い切れる箇所はありませんか)"),
    )
}

/// 行(=段落)ごとに `phrases` の一致数を数え、`min` 以上なら最初〜最後の一致を覆う Hint を返す。
#[instrument(skip(message), ret)]
fn per_line_density(
    tokens: &[STok],
    phrases: &[String],
    min: usize,
    rule: RuleId,
    message: impl Fn(usize) -> String,
) -> Vec<Finding> {
    if min == 0 {
        return Vec::new();
    }
    line_ranges(tokens)
        .into_iter()
        .filter_map(|r| {
            let hits = scan_phrases(tokens, r, phrases);
            (hits.len() >= min).then(|| Finding {
                rule,
                severity: Severity::Hint,
                message: message(hits.len()),
                range: token_range(tokens, hits[0].0, hits.last().unwrap().1),
            })
        })
        .collect()
}

/// 「〜すぎる」の連発(「美味しすぎ」「気持ちよすぎ」…)。出典: stop-ai-slop-jp phrases「○○すぎる連発」。
#[instrument(skip(tokens), ret)]
pub(crate) fn sugiru_repeat(tokens: &[STok], config: &QualityConfig) -> Vec<Finding> {
    let positions: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.pos() == "動詞"
                && t.sub1() == "非自立"
                && matches!(t.base(), "すぎる" | "過ぎる")
                && !t.in_dialogue()
        })
        .map(|(i, _)| i)
        .collect();
    cluster_matches(&positions, config.sugiru_window, config.sugiru_threshold)
        .into_iter()
        .map(|(a, b)| Finding {
            rule: RuleId::SugiruRepeat,
            severity: Severity::Hint,
            message: format!(
                "「〜すぎる」が近い範囲で{}回使われています(中間の温度の表現も混ぜられませんか)",
                b - a + 1
            ),
            range: token_range(tokens, positions[a], positions[b]),
        })
        .collect()
}

// ---- 記号の誤用(生テキスト。台詞も対象) ----

/// 三点リーダ・ダッシュを別の記号で代用していないか。
/// 「―」(U+2015)の代わりの「—」(U+2014)・「─」(U+2500)、「……」の代わりの「...」「・・・」。
/// 表記の問題で台詞かどうかに関係ないため、トークンではなく行の生テキストを見る。
#[instrument(skip(lines), ret)]
pub(crate) fn punct_char(lines: &[LineData]) -> Vec<Finding> {
    const TARGETS: &[(&str, &str)] = &[
        ("—", "「—」(U+2014)は「―」(U+2015)の誤用の可能性があります"),
        (
            "─",
            "罫線「─」(U+2500)がダッシュ「―」(U+2015)の代わりに使われている可能性があります",
        ),
        (".", "「...」は三点リーダ「……」にするのが出版慣行です"),
        ("・", "「・・・」は三点リーダ「……」にするのが出版慣行です"),
        ("．", "「．．．」は三点リーダ「……」にするのが出版慣行です"),
    ];

    let mut findings = Vec::new();
    for (line_no, line) in lines.iter().enumerate() {
        for &(target, message) in TARGETS {
            let ch = target.chars().next().expect("TARGETS は1文字");
            // em dash / 罫線は1個でも誤用。ドット類は2個以上の連続だけを見る(「3.5」「中黒の列挙」を避ける)。
            let min_run = if matches!(ch, '—' | '─') { 1 } else { 2 };
            for (start, end, count) in char_class_runs(&line.text, |c| c == ch) {
                if count < min_run {
                    continue;
                }
                findings.push(Finding {
                    rule: RuleId::PunctChar,
                    severity: Severity::Warning,
                    message: message.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{BracketColoring, Highlighter};
    use crate::quality::sentence::{build_tokens, split_sentences};
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

    fn build_all(text: &str) -> (Vec<LineData>, Vec<STok>, Vec<SentenceSpan>) {
        let lines = fold(text);
        let tokens = build_tokens(&lines);
        let sentences = split_sentences(&tokens);
        (lines, tokens, sentences)
    }

    // ---- grandiose_word ----

    #[test]
    fn test_grandiose_word_detects_cluster() {
        let (_, t, s) = build_all("そこには残酷な運命があった。冷徹な真実が彼を待っていた。");
        let f = grandiose_word(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
        assert_eq!(f[0].rule, RuleId::GrandioseWord);
    }

    #[test]
    fn test_grandiose_word_ok_for_single_use() {
        let (_, t, s) = build_all("彼は自分の運命を受け入れた。");
        assert!(grandiose_word(&t, &s, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_grandiose_word_ignored_in_dialogue() {
        let (_, t, s) = build_all("「残酷な運命と冷徹な真実だ」");
        assert!(grandiose_word(&t, &s, &QualityConfig::default()).is_empty());
    }

    // ---- pseudo_concrete / stock_phrase / translationese ----

    #[test]
    fn test_pseudo_concrete_detects_word() {
        let (_, t, s) = build_all("彼女の解像度が上がった。");
        assert_eq!(pseudo_concrete(&t, &s, &QualityConfig::default()).len(), 1);
    }

    #[test]
    fn test_pseudo_concrete_ok_for_plain_text() {
        let (_, t, s) = build_all("彼女は静かに笑った。");
        assert!(pseudo_concrete(&t, &s, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_stock_phrase_detects_across_token_boundaries() {
        let (_, t, s) = build_all("重要なのは、彼が生きていることだ。");
        let f = stock_phrase(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
        assert!(f[0].message.contains("重要なのは"));
    }

    #[test]
    fn test_stock_phrase_ignored_in_dialogue() {
        let (_, t, s) = build_all("「重要なのは覚悟だ」と彼は言った。");
        assert!(stock_phrase(&t, &s, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_translationese_detects_phrase() {
        let (_, t, s) = build_all("努力することによって道は開ける。");
        assert_eq!(translationese(&t, &s, &QualityConfig::default()).len(), 1);
    }

    #[test]
    fn test_translationese_detects_abstract_have() {
        let (_, t, s) = build_all("この決定は大きな意味を持つ。");
        let f = translationese(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_translationese_ok_for_concrete_have() {
        let (_, t, s) = build_all("彼は刀を持つ。");
        assert!(translationese(&t, &s, &QualityConfig::default()).is_empty());
    }

    // ---- inanimate_subject ----

    #[test]
    fn test_inanimate_subject_detects_pattern() {
        let (_, t, s) = build_all("この事実は従来の前提の誤りを示唆している。");
        let f = inanimate_subject(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_inanimate_subject_ok_for_human_subject() {
        let (_, t, s) = build_all("彼は事実を静かに受け止めた。");
        assert!(inanimate_subject(&t, &s, &QualityConfig::default()).is_empty());
    }

    // ---- antithesis_repeat ----

    #[test]
    fn test_antithesis_repeat_detects_three() {
        let (_, t, s) = build_all(
            "それは勝利ではなく、解放だった。彼は剣ではなく、言葉を選んだ。恐怖ではなく、安堵が胸を満たした。",
        );
        let f = antithesis_repeat(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_antithesis_repeat_ok_for_single() {
        let (_, t, s) = build_all("それは勝利ではなく、解放だった。");
        assert!(antithesis_repeat(&t, &s, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_antithesis_repeat_counts_negative_then_short_affirmation() {
        let (_, t, s) = build_all(
            "それは勝利ではなかった。解放だった。彼は逃げたのではなかった。選んだのだ。\
             恐怖ではなかった。安堵だった。",
        );
        let f = antithesis_repeat(&t, &s, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    // ---- sentence_start_repeat ----

    #[test]
    fn test_sentence_start_repeat_detects_three() {
        let (l, t, s) = build_all("彼は走った。彼は笑った。彼は黙った。");
        let f = sentence_start_repeat(&t, &s, &l, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
        assert_eq!(f[0].rule, RuleId::SentenceStartRepeat);
    }

    #[test]
    fn test_sentence_start_repeat_resets_across_paragraph_break() {
        let (l, t, s) = build_all("彼は走った。彼は笑った。\n\n彼は黙った。彼は泣いた。");
        assert!(sentence_start_repeat(&t, &s, &l, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_sentence_start_repeat_ok_for_varied_starts() {
        let (l, t, s) = build_all("彼は走った。空は晴れていた。風が吹いた。");
        assert!(sentence_start_repeat(&t, &s, &l, &QualityConfig::default()).is_empty());
    }

    // ---- fragment_run ----

    #[test]
    fn test_fragment_run_detects_three_short() {
        let (l, t, s) = build_all("静寂。それだけ。それが答え。");
        let f = fragment_run(&t, &s, &l, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_fragment_run_ok_for_long_sentences() {
        let (l, t, s) = build_all(
            "彼は長い夜を一人で過ごした。朝になっても誰も来なかった。窓の外では雨が降っていた。",
        );
        assert!(fragment_run(&t, &s, &l, &QualityConfig::default()).is_empty());
    }

    // ---- intensifier_density / hedge_stack / sugiru_repeat ----

    #[test]
    fn test_intensifier_density_detects_two_in_a_line() {
        let (_, t, _) = build_all("彼はとても静かで、本当に優しい人だった。");
        let f = intensifier_density(&t, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_intensifier_density_ok_for_one_per_line() {
        let (_, t, _) = build_all("彼はとても静かだった。\n彼女は本当に優しかった。");
        assert!(intensifier_density(&t, &QualityConfig::default()).is_empty());
    }

    #[test]
    fn test_hedge_stack_detects_three_in_a_line() {
        let (_, t, _) =
            build_all("雨が降るかもしれないし、遅れる可能性があるし、中止の場合がある。");
        let f = hedge_stack(&t, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_sugiru_repeat_detects_three() {
        let (_, t, _) = build_all("料理は美味しすぎた。景色は綺麗すぎた。彼女は優しすぎた。");
        let f = sugiru_repeat(&t, &QualityConfig::default());
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_sugiru_repeat_ok_for_motion_verb() {
        let (_, t, _) = build_all("時間が過ぎた。季節が過ぎた。夏が過ぎた。");
        assert!(sugiru_repeat(&t, &QualityConfig::default()).is_empty());
    }

    // ---- punct_char ----

    #[test]
    fn test_punct_char_detects_em_dash() {
        let lines = fold("彼は言った—待ってくれ。");
        assert_eq!(punct_char(&lines).len(), 1);
    }

    #[test]
    fn test_punct_char_detects_ascii_dots_even_in_dialogue() {
        let lines = fold("「え...そうなの」");
        let f = punct_char(&lines);
        assert_eq!(f.len(), 1, "{:?}", f);
    }

    #[test]
    fn test_punct_char_ok_for_proper_ellipsis_and_decimal() {
        let lines = fold("「え……そうなの」3.5倍だった。");
        assert!(punct_char(&lines).is_empty());
    }
}
