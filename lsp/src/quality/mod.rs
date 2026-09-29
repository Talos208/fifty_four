//! 形態素解析結果だけで判定できる文章品質診断(LLM 非依存)。
//!
//! `Highlighter`(lindera/IPADIC)が生成し `LineData.tokens` にキャッシュされた
//! 形態素トークン列だけを入力に、決定的ルール違反(表記上ほぼ確実な誤り・文章作法上の
//! 指摘)と文体統計の逸脱を検出し、`textDocument/publishDiagnostics` 用の `Finding` を返す。
//! LLM は一切呼ばない。
//!
//! このモジュールは `Backend`/`Client` に依存しない純粋関数の集合として設計してある
//! (入力は `&[LineData]` + `QualityConfig`、出力は `Vec<Finding>`)。LSP への配線
//! (`publishDiagnostics` への変換・debounce)は `backend.rs` 側が担う。
//!
//! # 前提条件
//! `analyze_document` を呼ぶ前に、`Highlighter::ensure_line_state` で対象行すべての
//! `tokens[].meaning`/`state_after` を畳み込み済みにしておくこと。畳み込まれていない行は
//! `tokens` が空のまま扱われ、その行に対する診断は単に出ない(誤検知はしない)。

mod rules;
#[cfg(test)]
pub(crate) use rules::bracket_mismatch;
mod sentence;
mod stats;

use crate::types::LineData;

/// 個々のルールを識別する ID。LSP `Diagnostic.code` にもこの `code()` を使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RuleId {
    SentenceEndRepeat,
    NoChain,
    TopicDuplicate,
    RedundantCanDo,
    RedundantToIu,
    IdropVerb,
    RaDrop,
    DoubleHonorific,
    KanjiFormalNoun,
    EllipsisPair,
    CommaSpan,
    WordRepeat,
    BracketMismatch,
    StyleMixed,
    SentenceTooLong,
    KanaRun,
    KanjiRun,
}

impl RuleId {
    pub(crate) fn code(self) -> &'static str {
        match self {
            RuleId::SentenceEndRepeat => "sentence-end-repeat",
            RuleId::NoChain => "no-chain",
            RuleId::TopicDuplicate => "topic-duplicate",
            RuleId::RedundantCanDo => "redundant-can-do",
            RuleId::RedundantToIu => "redundant-to-iu",
            RuleId::IdropVerb => "i-drop-verb",
            RuleId::RaDrop => "ra-drop",
            RuleId::DoubleHonorific => "double-honorific",
            RuleId::KanjiFormalNoun => "kanji-formal-noun",
            RuleId::EllipsisPair => "ellipsis-pair",
            RuleId::CommaSpan => "comma-span",
            RuleId::WordRepeat => "word-repeat",
            RuleId::BracketMismatch => "bracket-mismatch",
            RuleId::StyleMixed => "style-mixed",
            RuleId::SentenceTooLong => "sentence-too-long",
            RuleId::KanaRun => "kana-run",
            RuleId::KanjiRun => "kanji-run",
        }
    }
}

/// LSP の `DiagnosticSeverity` に対応する、このモジュール独自の重要度。
/// `lsp_types` への依存をこのモジュールに持ち込まないため独自定義にしてある
/// (`highlight::SemanticToken` が LSP 型を直接使わないのと同じ方針)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    /// 表記上ほぼ確実な誤り(括弧不一致、三点リーダの個数崩れ、い抜きなど)。
    Warning,
    /// 文章作法上の指摘(文末連続、「の」3連鎖、冗長表現など)。
    Information,
    /// 統計的逸脱(長文・かな/漢字の連続)。ノイズになりやすいので最弱。
    Hint,
}

/// バイトオフセットベースの範囲。`(start_line, start_byte, end_line, end_byte)`。
/// UTF-16 の `Range` への変換は呼び出し側(`backend.rs`)が `LineData.text` を使って行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FindingRange {
    pub start_line: usize,
    pub start_byte: usize,
    pub end_line: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Finding {
    pub rule: RuleId,
    pub severity: Severity,
    pub message: String,
    pub range: FindingRange,
}

/// ルールごとの閾値・on/off をまとめた設定。既定値は「誤検知が少ない」側に倒してある。
#[derive(Debug, Clone)]
pub(crate) struct QualityConfig {
    pub disabled: std::collections::HashSet<RuleId>,
    /// 文末の同一助動詞がこの回数以上連続したら指摘する。
    pub sentence_end_repeat_threshold: usize,
    /// 「の」の連体化用法が1文中にこの回数以上あれば指摘する。
    pub no_chain_threshold: usize,
    /// 係助詞「は」が1文中にこの回数以上あれば指摘する。
    pub topic_duplicate_threshold: usize,
    /// 「という」の近接反復を検出するウィンドウ幅(クラスタ先頭からの距離がトークン数でこの値以内)。
    pub to_iu_window: usize,
    /// 上記ウィンドウ内での出現回数閾値。
    pub to_iu_threshold: usize,
    /// true: 三点リーダ・ダッシュは「ちょうど2個」のみ許容。false: 偶数個なら許容。
    pub ellipsis_strict: bool,
    /// 読点なしで許容する最大文字数。
    pub comma_span_max_chars: usize,
    /// 自立語の近接反復を検出するウィンドウ幅(クラスタ先頭からの距離がトークン数でこの値以内)。
    pub word_repeat_window: usize,
    /// 上記ウィンドウ内での出現回数閾値。
    pub word_repeat_threshold: usize,
    /// 反復検出の対象にする表層形の最小文字数(短すぎる語のノイズを避ける)。
    pub word_repeat_min_chars: usize,
    /// 常体/敬体混在: 少数派の比率がこの値未満なら「混在」として指摘する。
    pub style_mixed_min_ratio: f32,
    /// 1文の文字数がこれを超えたら指摘する。
    pub sentence_too_long_chars: usize,
    /// ひらがなの連続がこれを超えたら指摘する。
    pub kana_run_max: usize,
    /// 漢字の連続がこれを超えたら指摘する。
    pub kanji_run_max: usize,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            disabled: std::collections::HashSet::new(),
            sentence_end_repeat_threshold: 3,
            no_chain_threshold: 3,
            topic_duplicate_threshold: 2,
            to_iu_window: 120,
            to_iu_threshold: 3,
            ellipsis_strict: true,
            comma_span_max_chars: 40,
            word_repeat_window: 30,
            word_repeat_threshold: 3,
            word_repeat_min_chars: 2,
            style_mixed_min_ratio: 0.1,
            sentence_too_long_chars: 100,
            kana_run_max: 15,
            kanji_run_max: 6,
        }
    }
}

impl QualityConfig {
    pub(crate) fn is_enabled(&self, rule: RuleId) -> bool {
        !self.disabled.contains(&rule)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct QualityReport {
    pub findings: Vec<Finding>,
    pub stats: stats::DocStats,
}

/// `lens` の標本数が `min_samples` 以上あれば「平均 + `k`×標準偏差」を、そうでなければ
/// `floor` をそのまま返す(サンプルが少ない文書で閾値が不安定にならないようにする)。
/// いずれの場合も `floor` を下回らない。
///
/// `kanji_run`/`comma_span` で使う: 固定の絶対閾値だけで判定すると、地の文で漢字を多用する
/// 硬めの文体や句読点を控えめに打つ文体の作者を毎回誤検知させてしまう。文書自身の分布を
/// 基準にすることで、「その作者にとっての通常運転」からの逸脱だけを指摘できる。
pub(crate) fn relative_threshold(lens: &[f32], floor: usize, min_samples: usize, k: f32) -> usize {
    if lens.len() < min_samples {
        return floor;
    }
    let mean = lens.iter().sum::<f32>() / lens.len() as f32;
    let variance = lens.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / lens.len() as f32;
    let std_dev = variance.sqrt();
    (floor as f32).max(mean + k * std_dev).round() as usize
}

/// `text.as_ref()..start..end` の文字種ランを `(start_byte, end_byte, count)` の列として返す。
/// `count` は文字数(バイト数ではない)。`ellipsis_pair`/`kana_run`/`kanji_run` の3つで共用する。
pub(crate) fn char_class_runs(text: &str, pred: impl Fn(char) -> bool) -> Vec<(usize, usize, usize)> {
    let mut runs = Vec::new();
    let mut iter = text.char_indices().peekable();
    while let Some((start, c)) = iter.next() {
        if !pred(c) {
            continue;
        }
        let mut end = start + c.len_utf8();
        let mut count = 1usize;
        while let Some(&(i, c2)) = iter.peek() {
            if pred(c2) {
                end = i + c2.len_utf8();
                count += 1;
                iter.next();
            } else {
                break;
            }
        }
        runs.push((start, end, count));
    }
    runs
}

/// 形態素解析結果だけを入力に文章品質を診断する。
///
/// `lines` はあらかじめ `Highlighter::ensure_line_state` で畳み込み済みであること
/// (モジュール冒頭のドキュメント参照)。
pub(crate) fn analyze_document(lines: &[LineData], config: &QualityConfig) -> QualityReport {
    let mut findings = Vec::new();

    // 生テキストを直接見るルール(トークン化されない記号を扱うため)。
    if config.is_enabled(RuleId::BracketMismatch) {
        findings.extend(rules::bracket_mismatch(lines));
    }
    if config.is_enabled(RuleId::EllipsisPair) {
        findings.extend(rules::ellipsis_pair(lines, config));
    }
    if config.is_enabled(RuleId::KanaRun) {
        findings.extend(stats::kana_run(lines, config));
    }
    if config.is_enabled(RuleId::KanjiRun) {
        findings.extend(stats::kanji_run(lines, config));
    }

    let tokens = sentence::build_tokens(lines);
    let sentences = sentence::split_sentences(&tokens);

    if config.is_enabled(RuleId::SentenceEndRepeat) {
        findings.extend(rules::sentence_end_repeat(&tokens, &sentences, lines, config));
    }
    if config.is_enabled(RuleId::NoChain) {
        findings.extend(rules::no_chain(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::TopicDuplicate) {
        findings.extend(rules::topic_duplicate(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::RedundantCanDo) {
        findings.extend(rules::redundant_can_do(&tokens, &sentences));
    }
    if config.is_enabled(RuleId::RedundantToIu) {
        findings.extend(rules::redundant_to_iu(&tokens, config));
    }
    if config.is_enabled(RuleId::IdropVerb) {
        findings.extend(rules::idrop_verb(&tokens));
    }
    if config.is_enabled(RuleId::RaDrop) {
        findings.extend(rules::ra_drop(&tokens));
    }
    if config.is_enabled(RuleId::DoubleHonorific) {
        findings.extend(rules::double_honorific(&tokens));
    }
    if config.is_enabled(RuleId::KanjiFormalNoun) {
        findings.extend(rules::kanji_formal_noun(&tokens));
    }
    if config.is_enabled(RuleId::CommaSpan) {
        findings.extend(rules::comma_span(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::WordRepeat) {
        findings.extend(rules::word_repeat(&tokens, config));
    }

    let doc_stats = stats::compute_stats(&tokens, &sentences);
    if config.is_enabled(RuleId::StyleMixed) {
        findings.extend(stats::style_mixed(&tokens, &sentences, &doc_stats, config));
    }
    if config.is_enabled(RuleId::SentenceTooLong) {
        findings.extend(stats::sentence_too_long(&tokens, &sentences, config));
    }

    QualityReport {
        findings,
        stats: doc_stats,
    }
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
    fn test_analyze_document_empty_is_quiet() {
        let lines = fold("");
        let report = analyze_document(&lines, &QualityConfig::default());
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn test_analyze_document_disabled_rule_is_skipped() {
        let lines = fold("彼は走った。彼は笑った。彼は黙った。");
        let mut config = QualityConfig::default();
        config.disabled.insert(RuleId::SentenceEndRepeat);
        let report = analyze_document(&lines, &config);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.rule == RuleId::SentenceEndRepeat),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn test_analyze_document_combines_multiple_rules() {
        let lines = fold("彼は走った。彼は笑った。彼は黙った。");
        let report = analyze_document(&lines, &QualityConfig::default());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule == RuleId::SentenceEndRepeat),
            "{:?}",
            report.findings
        );
    }
}
