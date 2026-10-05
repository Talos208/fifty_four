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

pub(crate) mod llm_review;
mod rules;
#[cfg(test)]
pub(crate) use rules::bracket_mismatch;
use tracing::instrument;
mod sentence;
mod slop;
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
    // ---- slop.rs(「AI が書いたような日本語」) ----
    GrandioseWord,
    PseudoConcrete,
    StockPhrase,
    Translationese,
    InanimateSubject,
    AntithesisRepeat,
    SentenceStartRepeat,
    FragmentRun,
    IntensifierDensity,
    SugiruRepeat,
    HedgeStack,
    PunctChar,
    // ---- llm_review.rs ----
    LlmReview,
}

impl RuleId {
    /// 設定の `disabled` に書けるルールコードの全一覧(`from_code` の逆引き用)。
    pub(crate) const ALL: &'static [RuleId] = &[
        RuleId::SentenceEndRepeat,
        RuleId::NoChain,
        RuleId::TopicDuplicate,
        RuleId::RedundantCanDo,
        RuleId::RedundantToIu,
        RuleId::IdropVerb,
        RuleId::RaDrop,
        RuleId::DoubleHonorific,
        RuleId::KanjiFormalNoun,
        RuleId::EllipsisPair,
        RuleId::CommaSpan,
        RuleId::WordRepeat,
        RuleId::BracketMismatch,
        RuleId::StyleMixed,
        RuleId::SentenceTooLong,
        RuleId::KanaRun,
        RuleId::KanjiRun,
        RuleId::GrandioseWord,
        RuleId::PseudoConcrete,
        RuleId::StockPhrase,
        RuleId::Translationese,
        RuleId::InanimateSubject,
        RuleId::AntithesisRepeat,
        RuleId::SentenceStartRepeat,
        RuleId::FragmentRun,
        RuleId::IntensifierDensity,
        RuleId::SugiruRepeat,
        RuleId::HedgeStack,
        RuleId::PunctChar,
        RuleId::LlmReview,
    ];

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
            RuleId::GrandioseWord => "grandiose-word",
            RuleId::PseudoConcrete => "pseudo-concrete",
            RuleId::StockPhrase => "stock-phrase",
            RuleId::Translationese => "translationese",
            RuleId::InanimateSubject => "inanimate-subject",
            RuleId::AntithesisRepeat => "antithesis-repeat",
            RuleId::SentenceStartRepeat => "sentence-start-repeat",
            RuleId::FragmentRun => "fragment-run",
            RuleId::IntensifierDensity => "intensifier-density",
            RuleId::SugiruRepeat => "sugiru-repeat",
            RuleId::HedgeStack => "hedge-stack",
            RuleId::PunctChar => "punct-char",
            RuleId::LlmReview => "llm-review",
        }
    }

    /// `code()` の逆引き。未知のコードは `None`。
    pub(crate) fn from_code(code: &str) -> Option<RuleId> {
        RuleId::ALL.iter().copied().find(|r| r.code() == code)
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

impl Severity {
    /// FlightRecorder(`quality_findings.severity`)に書く文字列。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Information => "information",
            Severity::Hint => "hint",
        }
    }
}

/// 指摘範囲の本文(FlightRecorder 用)。複数行にまたがる場合は各行の該当部分を連結する。
/// 長い場合は先頭 `max_chars` 文字で切る。範囲がテキスト外・文字境界の途中でも panic しない。
pub(crate) fn finding_excerpt(lines: &[LineData], r: FindingRange, max_chars: usize) -> String {
    let mut out = String::new();
    for line_no in r.start_line..=r.end_line.min(lines.len().saturating_sub(1)) {
        let Some(l) = lines.get(line_no) else { break };
        let clamp = |byte: usize| {
            let mut b = byte.min(l.text.len());
            while !l.text.is_char_boundary(b) {
                b -= 1;
            }
            b
        };
        let from = if line_no == r.start_line {
            clamp(r.start_byte)
        } else {
            0
        };
        let to = if line_no == r.end_line {
            clamp(r.end_byte)
        } else {
            l.text.len()
        };
        if from < to {
            out.push_str(&l.text[from..to]);
        }
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.chars().take(max_chars).collect()
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

/// ルールごとの閾値・語彙・on/off をまとめた設定。既定値は「誤検知が少ない」側に倒してある。
///
/// `initialization_options.quality.rules` の JSON から `QualityConfig::from_value` で読む。
/// 書かなかった項目は既定値になる(`#[serde(default)]`)。未知のキーは無視する。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub(crate) struct QualityConfig {
    /// 無効にするルールのコード(`RuleId::code`)。未知のコードは警告して無視する。
    #[serde(deserialize_with = "deserialize_disabled")]
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

    // ---- slop.rs ----
    /// 壮大化熟語を密集とみなすウィンドウ幅(トークン数)と出現回数。
    pub grandiose_window: usize,
    pub grandiose_threshold: usize,
    /// 「AではなくB」型の対比の反復を検出するウィンドウ幅(トークン数)と出現回数。
    pub antithesis_window: usize,
    pub antithesis_threshold: usize,
    /// 文頭(最初の2形態素)が同じ文がこの数以上連続したら指摘する。
    pub sentence_start_repeat_threshold: usize,
    /// この文字数以下の文を「短文」とし、`fragment_run` 文以上連続したら指摘する。
    pub fragment_max_chars: usize,
    pub fragment_run: usize,
    /// 1段落(1行)にこの回数以上あれば指摘する。
    pub intensifier_per_line: usize,
    pub hedge_per_line: usize,
    /// 「〜すぎる」の近接反復を検出するウィンドウ幅(トークン数)と出現回数。
    pub sugiru_window: usize,
    pub sugiru_threshold: usize,

    // ---- 語彙リスト(置き換え。足すだけなら `vocab_extra`) ----
    pub grandiose_words: Vec<String>,
    pub pseudo_concrete_words: Vec<String>,
    pub stock_phrases: Vec<String>,
    pub translationese_phrases: Vec<String>,
    pub abstract_have_nouns: Vec<String>,
    pub inanimate_subjects: Vec<String>,
    pub inanimate_verbs: Vec<String>,
    pub intensifiers: Vec<String>,
    pub hedges: Vec<String>,
    /// 既定(または置き換え後)の語彙リストへ追加する語。キーは上記のリスト名
    /// (例 `{"grandiose_words": ["比類なき"]}`)。
    pub vocab_extra: std::collections::HashMap<String, Vec<String>>,
}

fn to_strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
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
            grandiose_window: 200,
            grandiose_threshold: 3,
            antithesis_window: 400,
            antithesis_threshold: 3,
            sentence_start_repeat_threshold: 3,
            fragment_max_chars: 10,
            fragment_run: 3,
            intensifier_per_line: 2,
            hedge_per_line: 3,
            sugiru_window: 300,
            sugiru_threshold: 3,
            grandiose_words: to_strings(slop::DEFAULT_GRANDIOSE_WORDS),
            pseudo_concrete_words: to_strings(slop::DEFAULT_PSEUDO_CONCRETE_WORDS),
            stock_phrases: to_strings(slop::DEFAULT_STOCK_PHRASES),
            translationese_phrases: to_strings(slop::DEFAULT_TRANSLATIONESE_PHRASES),
            abstract_have_nouns: to_strings(slop::DEFAULT_ABSTRACT_HAVE_NOUNS),
            inanimate_subjects: to_strings(slop::DEFAULT_INANIMATE_SUBJECTS),
            inanimate_verbs: to_strings(slop::DEFAULT_INANIMATE_VERBS),
            intensifiers: to_strings(slop::DEFAULT_INTENSIFIERS),
            hedges: to_strings(slop::DEFAULT_HEDGES),
            vocab_extra: std::collections::HashMap::new(),
        }
    }
}

/// `disabled` 用: ルールコードの配列を `RuleId` の集合へ。未知のコードは警告して捨てる。
fn deserialize_disabled<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashSet<RuleId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let codes = Vec::<String>::deserialize(deserializer)?;
    Ok(codes
        .into_iter()
        .filter_map(|c| {
            let rule = RuleId::from_code(&c);
            if rule.is_none() {
                log::warn!("quality: 未知のルールコード {c:?} を disabled から無視します");
            }
            rule
        })
        .collect())
}

impl QualityConfig {
    pub(crate) fn is_enabled(&self, rule: RuleId) -> bool {
        !self.disabled.contains(&rule)
    }

    /// `quality.rules` の JSON から設定を作る。不正な JSON(型違い等)は警告して既定値を返す
    /// (設定ミスで診断が丸ごと止まらないようにする)。
    pub(crate) fn from_value(value: &serde_json::Value) -> Self {
        let mut config = match serde_json::from_value::<QualityConfig>(value.clone()) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("quality.rules の読み込みに失敗したため既定値を使います: {e}");
                return Self::default();
            }
        };
        config.apply_vocab_extra();
        config
    }

    /// `vocab_extra` を各語彙リストへ合流させる。未知のリスト名は警告して無視する。
    fn apply_vocab_extra(&mut self) {
        let extra = std::mem::take(&mut self.vocab_extra);
        for (name, words) in extra {
            let target = match name.as_str() {
                "grandiose_words" => &mut self.grandiose_words,
                "pseudo_concrete_words" => &mut self.pseudo_concrete_words,
                "stock_phrases" => &mut self.stock_phrases,
                "translationese_phrases" => &mut self.translationese_phrases,
                "abstract_have_nouns" => &mut self.abstract_have_nouns,
                "inanimate_subjects" => &mut self.inanimate_subjects,
                "inanimate_verbs" => &mut self.inanimate_verbs,
                "intensifiers" => &mut self.intensifiers,
                "hedges" => &mut self.hedges,
                _ => {
                    log::warn!("quality.rules.vocab_extra: 未知のリスト名 {name:?} を無視します");
                    continue;
                }
            };
            for w in words {
                if !target.contains(&w) {
                    target.push(w);
                }
            }
        }
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
pub(crate) fn char_class_runs(
    text: &str,
    pred: impl Fn(char) -> bool,
) -> Vec<(usize, usize, usize)> {
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

/// 指摘の同一性キー(ルールコード + 範囲の本文)。行番号を含めないので、上の行を編集して
/// 行がずれても同じ指摘とみなせる。FlightRecorder の重複記録の抑制に使う。
pub(crate) fn finding_key(rule: RuleId, excerpt: &str) -> u64 {
    farmhash::hash64(format!("{}\u{0}{}", rule.code(), excerpt).as_bytes())
}

/// 今回の `findings` のうち、前回(`prev`)には無かったものだけを `(指摘, 抜粋)` で返し、
/// 今回のキー集合も返す。呼び出し側は戻り値のキー集合を次回の `prev` として保存する。
///
/// 現在の集合で置き換える(累積しない)ので、直した指摘がもう一度現れたら再び「新規」になり、
/// 集合が無限に膨らむこともない。同じキー(同じルール・同じ本文)は1回だけ数える。
pub(crate) fn new_findings<'a>(
    prev: &std::collections::HashSet<u64>,
    findings: &'a [Finding],
    lines: &[LineData],
) -> (std::collections::HashSet<u64>, Vec<(&'a Finding, String)>) {
    let mut current = std::collections::HashSet::new();
    let mut fresh = Vec::new();
    for f in findings {
        let excerpt = finding_excerpt(lines, f.range, 60);
        let key = finding_key(f.rule, &excerpt);
        if current.insert(key) && !prev.contains(&key) {
            fresh.push((f, excerpt));
        }
    }
    (current, fresh)
}

/// 形態素解析結果だけを入力に文章品質を診断する。
///
/// `lines` はあらかじめ `Highlighter::ensure_line_state` で畳み込み済みであること
/// (モジュール冒頭のドキュメント参照)。
#[instrument(skip(lines), ret)]
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
        findings.extend(rules::sentence_end_repeat(
            &tokens, &sentences, lines, config,
        ));
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

    // 「AI が書いたような日本語」(slop.rs)。地の文のみ。
    if config.is_enabled(RuleId::PunctChar) {
        findings.extend(slop::punct_char(lines));
    }
    if config.is_enabled(RuleId::GrandioseWord) {
        findings.extend(slop::grandiose_word(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::PseudoConcrete) {
        findings.extend(slop::pseudo_concrete(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::StockPhrase) {
        findings.extend(slop::stock_phrase(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::Translationese) {
        findings.extend(slop::translationese(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::InanimateSubject) {
        findings.extend(slop::inanimate_subject(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::AntithesisRepeat) {
        findings.extend(slop::antithesis_repeat(&tokens, &sentences, config));
    }
    if config.is_enabled(RuleId::SentenceStartRepeat) {
        findings.extend(slop::sentence_start_repeat(
            &tokens, &sentences, lines, config,
        ));
    }
    if config.is_enabled(RuleId::FragmentRun) {
        findings.extend(slop::fragment_run(&tokens, &sentences, lines, config));
    }
    if config.is_enabled(RuleId::IntensifierDensity) {
        findings.extend(slop::intensifier_density(&tokens, config));
    }
    if config.is_enabled(RuleId::SugiruRepeat) {
        findings.extend(slop::sugiru_repeat(&tokens, config));
    }
    if config.is_enabled(RuleId::HedgeStack) {
        findings.extend(slop::hedge_stack(&tokens, config));
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
    fn test_config_from_value_partial_keeps_defaults() {
        let c = QualityConfig::from_value(&serde_json::json!({"fragment_run": 5}));
        assert_eq!(c.fragment_run, 5);
        assert_eq!(
            c.no_chain_threshold,
            QualityConfig::default().no_chain_threshold
        );
        assert!(!c.grandiose_words.is_empty());
    }

    #[test]
    fn test_config_from_value_disabled_ignores_unknown_code() {
        let c = QualityConfig::from_value(
            &serde_json::json!({"disabled": ["fragment-run", "no-such-rule"]}),
        );
        assert!(!c.is_enabled(RuleId::FragmentRun));
        assert_eq!(c.disabled.len(), 1);
    }

    #[test]
    fn test_config_from_value_vocab_replace_and_extra() {
        let c = QualityConfig::from_value(&serde_json::json!({
            "grandiose_words": ["比類なき"],
            "vocab_extra": {"grandiose_words": ["至高", "比類なき"], "hedges": ["かも"], "bogus": ["x"]}
        }));
        assert_eq!(c.grandiose_words, vec!["比類なき", "至高"]);
        assert!(c.hedges.iter().any(|h| h == "かも"));
        assert!(c.hedges.len() > 1, "既定のヘッジ語彙は残る");
    }

    #[test]
    fn test_config_from_value_invalid_type_falls_back_to_default() {
        let c = QualityConfig::from_value(&serde_json::json!({"fragment_run": "many"}));
        assert_eq!(c.fragment_run, QualityConfig::default().fragment_run);
    }

    #[test]
    fn test_finding_excerpt_single_multi_line_and_out_of_range() {
        let lines = fold("あいうえお\nかきくけこ");
        let r = |sl, sb, el, eb| FindingRange {
            start_line: sl,
            start_byte: sb,
            end_line: el,
            end_byte: eb,
        };
        assert_eq!(finding_excerpt(&lines, r(0, 3, 0, 9), 40), "いう");
        assert_eq!(finding_excerpt(&lines, r(0, 9, 1, 3), 40), "えおか");
        assert_eq!(finding_excerpt(&lines, r(0, 0, 1, 15), 4), "あいうえ");
        // 文字境界の途中・範囲外でも panic しない
        assert_eq!(finding_excerpt(&lines, r(0, 1, 0, 100), 40), "あいうえお");
        assert_eq!(finding_excerpt(&lines, r(5, 0, 5, 3), 40), "");
    }

    #[test]
    fn test_new_findings_reports_only_new_and_survives_line_shift() {
        let config = QualityConfig::default();
        let first = fold("彼は走った。彼は笑った。彼は黙った。");
        let report1 = analyze_document(&first, &config);
        let (keys1, fresh1) = new_findings(&HashSet::new(), &report1.findings, &first);
        assert!(!fresh1.is_empty());
        assert_eq!(keys1.len(), fresh1.len());

        // 同じ内容のまま上に段落を足して行番号がずれても、新規としては出ない(空行で段落を分け、短文の連続に混ざらないようにする)
        let shifted = fold("プロローグ。\n\n彼は走った。彼は笑った。彼は黙った。");
        let report2 = analyze_document(&shifted, &config);
        let (keys2, fresh2) = new_findings(&keys1, &report2.findings, &shifted);
        assert!(fresh2.is_empty(), "{:?}", fresh2);
        assert_eq!(keys1, keys2);

        // 指摘が消えたあとの集合は空になり、再び現れたら新規扱いに戻る
        let fixed = fold("彼は走った。");
        let report3 = analyze_document(&fixed, &config);
        let (keys3, _) = new_findings(&keys2, &report3.findings, &fixed);
        assert!(keys3.is_empty());
        let (_, fresh4) = new_findings(&keys3, &report1.findings, &first);
        assert_eq!(fresh4.len(), fresh1.len());
    }

    #[test]
    fn test_rule_id_code_roundtrip() {
        for &r in RuleId::ALL {
            assert_eq!(RuleId::from_code(r.code()), Some(r));
        }
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
