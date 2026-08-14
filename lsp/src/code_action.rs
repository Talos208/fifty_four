//! `textDocument/codeAction`(※穴埋め / 表現改善)の純粋ロジック。
//!
//! LSP クライアントや LLM を一切触らず、`Vec<LineData>` と `Range` だけを扱う関数を
//! まとめる(`cursor_context.rs` / `text.rs` と同じ方針)。LLM 呼び出し・進捗表示・
//! CodeAction 組み立ては `backend.rs` の `code_action` ハンドラが担う。

use crate::types::LineData;
use std::sync::Arc;
use tokio::task::AbortHandle;
use tower_lsp_server::lsp_types::{Position, Range, Uri};
use tracing::instrument;

/// 「↻ 候補を作り直す」の `workspace/executeCommand` コマンド名。
pub(crate) const REGENERATE_COMMAND: &str = "fifty_four.codeActionRegenerate";

/// `REGENERATE_COMMAND` の引数(`Command.arguments[0]` に1個だけ積む)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RegenerateArgs {
    pub(crate) uri: Uri,
    pub(crate) range: Range,
}

/// 対象範囲に応じた code action の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionMode {
    /// 対象内に `※` があった場合。`mark` はその1文字分の Range。
    FillMark { mark: Range },
    /// `※` が無かった場合。対象全体を言い換える。
    Rephrase,
}

/// `code_action` ジョブの同一性キー。
///
/// Zed の shortcut(`editor: toggle code actions`)は LSP へ新規リクエストを送らず、
/// 選択変更のたびの自動ポーリングが `code_actions_for_selection` に置いた結果を
/// 表示するだけ(Zed本体 `crates/editor/src/code_actions.rs` の `toggle_code_actions`)。
/// つまり「同一選択への2回目のリクエスト」は基本的に来ない。よって「1回目は記録だけ」
/// という判別は成立せず、**最初のリクエストで即座に LLM を起動する**。
///
/// その代わり、同一キー(選択範囲・対象テキストが同じ)への後続リクエストは新規に
/// LLM を呼ばず、進行中/完了済みのジョブに合流する(`decide_job` 参照)。これにより
/// Zed が同じ選択に対して複数回リクエストを送ってきても二重に LLM を呼ばず、かつ
/// 完了済みの結果はキャッシュとして再利用される。
///
/// `target_text` そのものではなくハッシュを持つのは、キーの比較・保持を軽くするため
/// (document version は持たない設計なので、テキストが変われば別ジョブとして扱われれば十分)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobKey {
    pub(crate) range: Range,
    pub(crate) target_hash: u64,
}

impl JobKey {
    pub(crate) fn new(range: Range, target_text: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        target_text.hash(&mut hasher);
        Self {
            range,
            target_hash: hasher.finish(),
        }
    }
}

/// URI ごとに進行中/完了済みの LLM 呼び出しを1つだけ保持する。
///
/// LLM 呼び出しは detached task に切り出しているため、これを保持している間はリクエストが
/// `$/cancelRequest` で drop されても task は生き続け、`rx` を clone した後続のリクエストが
/// 結果を拾える。
#[derive(Debug)]
pub(crate) struct RunningJob {
    pub(crate) rx: tokio::sync::watch::Receiver<Option<Arc<Vec<String>>>>,
    pub(crate) abort: AbortHandle,
}

/// `decide_job` の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// LLM を新規に起動する(別の選択、またはこの URI で初めての呼び出し)。
    Start,
    /// 既に起動済み(または完了済み)のジョブに合流する。
    Join,
}

/// 直前のジョブのキーと今回のキーから、LLM を新規に呼ぶか既存ジョブに合流するかを判定する。
#[instrument]
pub(crate) fn decide_job(prev_key: Option<&JobKey>, now_key: &JobKey) -> Decision {
    if prev_key == Some(now_key) {
        Decision::Join
    } else {
        Decision::Start
    }
}

/// `range` の直後の1行の utf16 終端位置を返す(範囲内の最終行に使う)。
fn line_end_char(text: &str) -> u32 {
    crate::types::utf16_len(text) as u32
}

/// `Range` が指すテキストを取り出す。複数行にまたがる場合は `\n` で連結する
/// (`apply_changes` 等が改行を独立した行区切りとして扱う実装と揃える)。
#[instrument]
pub(crate) fn slice_range(texts: &[LineData], range: Range) -> String {
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;

    let mut out = String::new();
    for line_no in start_line..=end_line {
        let Some(line) = texts.get(line_no) else {
            break;
        };

        let start_char = if line_no == start_line {
            range.start.character as usize
        } else {
            0
        };
        let end_char = if line_no == end_line {
            range.end.character as usize
        } else {
            line_end_char(&line.text) as usize
        };

        let start_byte = crate::types::utf16_to_byte_offset(&line.text, start_char);
        let end_byte = crate::types::utf16_to_byte_offset(&line.text, end_char);

        if line_no != start_line {
            out.push('\n');
        }
        out.push_str(&line.text[start_byte..end_byte]);
    }
    out
}

/// `range` 内で最初に現れる `※` の1文字分の `Range` を返す。
/// 複数ある場合は先頭のみを対象とする(残りは呼び出し元がログに出す想定。
/// 1回の code action では1箇所ずつ埋める運用)。
#[instrument]
pub(crate) fn find_mark(texts: &[LineData], range: Range) -> Option<Range> {
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;

    for line_no in start_line..=end_line {
        let line = texts.get(line_no)?;

        let start_char = if line_no == start_line {
            range.start.character as usize
        } else {
            0
        };
        let end_char = if line_no == end_line {
            range.end.character as usize
        } else {
            line_end_char(&line.text) as usize
        };

        let start_byte = crate::types::utf16_to_byte_offset(&line.text, start_char);
        let end_byte = crate::types::utf16_to_byte_offset(&line.text, end_char);

        if let Some(rel_byte) = line.text[start_byte..end_byte].find('※') {
            let byte_ix = start_byte + rel_byte;
            // '※'(U+203B)は BMP 文字なので UTF-16 コード単位は常に1。
            let char_ix = crate::types::utf16_len(&line.text[..byte_ix]) as u32;
            return Some(Range::new(
                Position::new(line_no as u32, char_ix),
                Position::new(line_no as u32, char_ix + 1),
            ));
        }
    }
    None
}

/// 対象範囲内に `※` があれば `FillMark`、無ければ `Rephrase` を選ぶ。
#[instrument]
pub(crate) fn decide_mode(texts: &[LineData], range: Range) -> ActionMode {
    match find_mark(texts, range) {
        Some(mark) => ActionMode::FillMark { mark },
        None => ActionMode::Rephrase,
    }
}

/// LLM 応答を候補の列へ分解する。
///
/// プロンプトは frontmatter の `schema` で `{"candidates": ["...", "..."]}` を要求する
/// (`use_llm_with_option` が構造化出力 / プロンプト埋め込みのどちらでも面倒を見る)。
/// **候補内の改行を保つためにこの形式が必要**で、補完が使っている
/// `extract_candidate_lines` のような行分割だと、複数行にわたる書き換えが
/// 別々の候補にバラされてしまう。
///
/// JSON として読めなかった場合(スキーマを無視するモデル等)は行分割へフォールバック
/// する。その場合だけは複数行の候補を表現できない。
#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
#[instrument]
pub(crate) fn parse_candidates(response: &str) -> Vec<String> {
    if let Some(list) = parse_candidates_json(response) {
        return list;
    }
    log::debug!("parse_candidates: not JSON, falling back to line split");
    crate::cursor_context::extract_candidate_lines(response)
        .into_iter()
        .map(|s| s.to_string())
        .collect()
}

/// `{"candidates": [...]}` 形式のパース。候補が1件も取れなければ `None`
/// (呼び出し元が行分割へフォールバックできるようにするため)。
#[instrument]
fn parse_candidates_json(response: &str) -> Option<Vec<String>> {
    let json = crate::text::extract_json(response)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let items = value.get("candidates")?.as_array()?;
    let list: Vec<String> = items
        .iter()
        .filter_map(|v| v.as_str())
        // 候補「内部」の改行は保つが、前後の空白・改行は落とす。
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn lines(text: &str) -> Vec<LineData> {
        text.lines()
            .map(|l| LineData::from_str(l).unwrap())
            .collect()
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range::new(Position::new(sl, sc), Position::new(el, ec))
    }

    // ---- slice_range ----

    #[test]
    fn test_slice_range_single_line() {
        let texts = lines("これは文章です。");
        let r = range(0, 2, 0, 4); // "は文"
        assert_eq!(slice_range(&texts, r), "は文");
    }

    #[test]
    fn test_slice_range_multi_line_joins_with_newline() {
        let texts = lines("一行目\n二行目\n三行目");
        let r = range(0, 1, 2, 2); // "行目\n二行目\n三行"
        assert_eq!(slice_range(&texts, r), "行目\n二行目\n三行");
    }

    // ---- find_mark ----

    #[test]
    fn test_find_mark_single_occurrence() {
        let texts = lines("彼は※と言った。");
        let r = range(0, 0, 0, texts[0].text.chars().count() as u32);
        let found = find_mark(&texts, r).expect("※ should be found");
        assert_eq!(found, range(0, 2, 0, 3));
        assert_eq!(slice_range(&texts, found), "※");
    }

    #[test]
    fn test_find_mark_none_when_absent() {
        let texts = lines("彼はそうと言った。");
        let r = range(0, 0, 0, texts[0].text.chars().count() as u32);
        assert_eq!(find_mark(&texts, r), None);
    }

    #[test]
    fn test_find_mark_returns_first_of_multiple() {
        let texts = lines("※は※だ。");
        let r = range(0, 0, 0, texts[0].text.chars().count() as u32);
        let found = find_mark(&texts, r).expect("※ should be found");
        assert_eq!(found, range(0, 0, 0, 1));
    }

    #[test]
    fn test_find_mark_outside_range_not_matched() {
        // 対象範囲の外にある ※ は無視される。
        let texts = lines("※これは対象外、これが対象");
        let full_len = texts[0].text.chars().count() as u32;
        let r = range(0, 2, 0, full_len); // "※" を含まない範囲
        assert_eq!(find_mark(&texts, r), None);
    }

    #[test]
    fn test_find_mark_crosses_lines() {
        let texts = lines("一行目\n※二行目");
        let r = range(0, 0, 1, texts[1].text.chars().count() as u32);
        let found = find_mark(&texts, r).expect("※ should be found");
        assert_eq!(found, range(1, 0, 1, 1));
    }

    // ---- decide_mode ----

    #[test]
    fn test_decide_mode_fill_mark_when_present() {
        let texts = lines("彼は※と言った。");
        let r = range(0, 0, 0, texts[0].text.chars().count() as u32);
        match decide_mode(&texts, r) {
            ActionMode::FillMark { mark } => assert_eq!(mark, range(0, 2, 0, 3)),
            ActionMode::Rephrase => panic!("expected FillMark"),
        }
    }

    #[test]
    fn test_decide_mode_rephrase_when_absent() {
        let texts = lines("彼はそうと言った。");
        let r = range(0, 0, 0, texts[0].text.chars().count() as u32);
        assert_eq!(decide_mode(&texts, r), ActionMode::Rephrase);
    }

    // ---- parse_candidates ----

    #[test]
    fn test_parse_candidates_json_preserves_newlines_within_candidate() {
        // 本題: 複数行の書き換えが1つの候補として保たれること
        // (行分割だと "一行目" と "二行目" が別候補にバラされてしまう)。
        let response = r#"{"candidates": ["一行目です。\n二行目です。", "別案です。"]}"#;
        assert_eq!(
            parse_candidates(response),
            vec!["一行目です。\n二行目です。", "別案です。"]
        );
    }

    #[test]
    fn test_parse_candidates_json_inside_code_fence() {
        let response = "以下が結果です:\n```json\n{\"candidates\": [\"候補1\", \"候補2\"]}\n```";
        assert_eq!(parse_candidates(response), vec!["候補1", "候補2"]);
    }

    #[test]
    fn test_parse_candidates_json_drops_empty_and_trims() {
        let response = r#"{"candidates": ["  候補1  ", "", "   "]}"#;
        assert_eq!(parse_candidates(response), vec!["候補1"]);
    }

    #[test]
    fn test_parse_candidates_falls_back_to_line_split() {
        // スキーマを無視して素の行を返すモデルへのフォールバック。
        let response = "候補1\n候補2\n候補3";
        assert_eq!(parse_candidates(response), vec!["候補1", "候補2", "候補3"]);
    }

    #[test]
    fn test_parse_candidates_json_without_candidates_key_falls_back() {
        // JSON ではあるが期待キーが無い場合も行分割へ倒す(空リストで詰まらせない)。
        let response = r#"{"result": ["候補1"]}"#;
        assert!(!parse_candidates(response).is_empty());
    }

    // ---- decide_job ----

    fn key(sc: u32, text: &str) -> JobKey {
        JobKey::new(range(0, sc, 0, sc + 1), text)
    }

    #[test]
    fn test_decide_job_first_request_starts() {
        let now_key = key(0, "対象");
        assert_eq!(decide_job(None, &now_key), Decision::Start);
    }

    #[test]
    fn test_decide_job_same_key_joins() {
        let k = key(0, "対象");
        assert_eq!(decide_job(Some(&k), &k), Decision::Join);
    }

    #[test]
    fn test_decide_job_range_changed_starts() {
        let prev_key = key(0, "対象");
        let now_key = key(5, "対象"); // 範囲が違う
        assert_eq!(decide_job(Some(&prev_key), &now_key), Decision::Start);
    }

    #[test]
    fn test_decide_job_text_edited_same_range_starts() {
        let prev_key = key(0, "対象");
        let now_key = key(0, "編集後"); // 範囲は同じだがテキストが変わった
        assert_eq!(decide_job(Some(&prev_key), &now_key), Decision::Start);
    }
}
