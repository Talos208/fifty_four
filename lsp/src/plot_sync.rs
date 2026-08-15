//! plot.md の `# 章名` 見出しと `<章名>.txt` のリネーム同期。
//!
//! 純粋ロジック(`diff_chapter_names` / `is_valid_chapter_file_name`)と、非同期ワーカー
//! (`run_after` / `run` / `rename_chapter_file`)を分ける方針は `character_updater.rs` と同じ。
//! `Backend` を直接渡さず、clone 可能な部品(`Client` / `Arc<DashMap<..>>` 等)だけを受け取る。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
#[allow(unused_imports)]
use log::{debug, error, warn};
use tower_lsp_server::Client;
use tower_lsp_server::lsp_types::*;
use tracing::instrument;

use crate::types::LineData;

/// `did_change` を検知してから実際にリネームを実行するまでの既定の待ち時間。
/// タイピング中の中間状態(例: 「第1」まで打った直後)で連続リネームしないための debounce。
pub(crate) const DEFAULT_PLOT_SYNC_IDLE_MS: u64 = 1500;

/// `pending_self_renames` のエントリを保持する上限時間。クライアントが
/// `workspace/didRenameFiles` を送らない実装だった場合に無期限に溜まり続けないようにする。
const SELF_RENAME_TTL: Duration = Duration::from_secs(10);

/// plot.md の URI ごとに保持するリネーム同期の状態。
#[derive(Debug)]
pub(crate) struct PlotSyncState {
    /// `did_change`/`did_save` のたびに増やすカウンタ。debounce タイマーが発火した時点で
    /// 最新の generation でなければ「後発の変更に追い越された」として何もしない。
    pub generation: u64,
    /// 「ディスク上の `.txt` 群と同期が取れている」と信じている章名の並び。
    pub baseline: Vec<String>,
    /// 順方向(plot.md → txt)でこちらから実行したリネーム。逆方向のハンドラが
    /// `workspace/didRenameFiles` で同じペアを受け取ったら自己エコーとして無視する。
    pub pending_self_renames: Vec<(PathBuf, PathBuf, Instant)>,
}

impl PlotSyncState {
    pub(crate) fn new(baseline: Vec<String>) -> Self {
        Self {
            generation: 0,
            baseline,
            pending_self_renames: Vec::new(),
        }
    }

    /// リネーム実行の直前に呼ぶ。`did_rename_files` 側の自己エコー遮断に使う。
    pub(crate) fn note_self_rename(&mut self, old: PathBuf, new: PathBuf) {
        let now = Instant::now();
        self.pending_self_renames
            .retain(|(_, _, at)| now.duration_since(*at) < SELF_RENAME_TTL);
        self.pending_self_renames.push((old, new, now));
    }

    /// `did_rename_files` から呼ぶ。一致するエントリがあれば取り除いて `true` を返す
    /// (=自己エコーなので無視してよい)。
    pub(crate) fn take_self_rename(
        &mut self,
        old: &std::path::Path,
        new: &std::path::Path,
    ) -> bool {
        let now = Instant::now();
        self.pending_self_renames
            .retain(|(_, _, at)| now.duration_since(*at) < SELF_RENAME_TTL);
        if let Some(ix) = self
            .pending_self_renames
            .iter()
            .position(|(o, n, _)| o == old && n == new)
        {
            self.pending_self_renames.remove(ix);
            true
        } else {
            false
        }
    }
}

/// `diff_chapter_names` の判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenamePlan {
    /// 差分なし。何もしない。
    Noop,
    /// 単一の章名変更として確定。リネームを実行する。
    Rename { old: String, new: String },
    /// 曖昧、または不正な変更。同期はスキップし、baseline だけ現状へ合わせる
    /// (次回以降も同じ差分で毎回判定し直さないため)。
    Rebaseline,
}

/// baseline と current(章名の並び、出現順)を比較し、単一の章名変更だけを `Rename` として
/// 確定する。位置(インデックス)一致で判定するため、章の並べ替えは複数箇所差分になり
/// 自動的に `Rebaseline` へ落ちる(集合差分だと並べ替えを改名と誤認するため、意図的にこの方式)。
#[instrument(skip(baseline, current))]
pub(crate) fn diff_chapter_names(baseline: &[String], current: &[String]) -> RenamePlan {
    if baseline.len() != current.len() {
        return RenamePlan::Rebaseline;
    }

    let diff_indices: Vec<usize> = baseline
        .iter()
        .zip(current.iter())
        .enumerate()
        .filter_map(|(i, (b, c))| (b != c).then_some(i))
        .collect();

    match diff_indices.as_slice() {
        [] => RenamePlan::Noop,
        [ix] => {
            let old = &baseline[*ix];
            let new = &current[*ix];
            if new.trim().is_empty()
                || !is_valid_chapter_file_name(new)
                || has_duplicates(baseline)
                || has_duplicates(current)
            {
                RenamePlan::Rebaseline
            } else {
                RenamePlan::Rename {
                    old: old.clone(),
                    new: new.clone(),
                }
            }
        }
        _ => RenamePlan::Rebaseline,
    }
}

fn has_duplicates(names: &[String]) -> bool {
    let mut seen = HashSet::new();
    names.iter().any(|n| !seen.insert(n.as_str()))
}

/// 章名を `<章名>.txt` というファイル名として使えるかを判定する。
/// Windows のファイル名制約(禁止文字・予約名・末尾のドット/空白)を弾く。
#[instrument]
pub(crate) fn is_valid_chapter_file_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // 前後に空白が付いている(=trim済みでない)場合も不可。呼び出し元は trim 済みの名前を
    // 渡す想定だが、この関数単体でも安全側に倒す。
    if name.trim() != name {
        return false;
    }
    if name.ends_with('.') {
        return false;
    }

    const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if name
        .chars()
        .any(|c| INVALID_CHARS.contains(&c) || c.is_control())
    {
        return false;
    }

    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return false;
    }

    true
}

/// `note_plot_change`(`Backend`)から spawn される debounce タイマー。`idle` 経過後、
/// 自分の `generation` が最新のままなら `run` 本体を実行する(後発の変更に追い越されて
/// いれば何もしない)。
#[allow(clippy::too_many_arguments)]
#[instrument(skip(text, client, state))]
pub(crate) async fn run_after(
    idle: Duration,
    generation: u64,
    plot_uri: Uri,
    workspace: PathBuf,
    text: Arc<DashMap<String, Vec<LineData>>>,
    client: Client,
    state: Arc<parking_lot::Mutex<PlotSyncState>>,
    supports_rename_op: bool,
) {
    tokio::time::sleep(idle).await;
    if state.lock().generation != generation {
        debug!(
            "plot_sync: superseded by a later change, skip (uri={})",
            plot_uri.as_str()
        );
        return;
    }
    run(
        generation,
        &plot_uri,
        &workspace,
        &text,
        &client,
        &state,
        supports_rename_op,
    )
    .await;
}

/// debounce を待たず即座に判定・実行する本体。`did_save` と `run_after` の両方から呼ぶ。
#[allow(clippy::too_many_arguments)]
#[instrument(skip(text, client, state))]
pub(crate) async fn run(
    generation: u64,
    plot_uri: &Uri,
    workspace: &std::path::Path,
    text: &DashMap<String, Vec<LineData>>,
    client: &Client,
    state: &parking_lot::Mutex<PlotSyncState>,
    supports_rename_op: bool,
) {
    let Some(lines) = text.get(plot_uri.as_str()) else {
        return;
    };
    let content = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    drop(lines);

    let current: Vec<String> = crate::plot::parse_plot(&content)
        .chapters
        .into_iter()
        .map(|c| c.name)
        .collect();

    let plan = {
        let s = state.lock();
        if s.generation != generation {
            debug!("plot_sync: superseded before diffing, skip");
            return;
        }
        diff_chapter_names(&s.baseline, &current)
    };

    match plan {
        RenamePlan::Noop => {}
        RenamePlan::Rebaseline => {
            state.lock().baseline = current;
        }
        RenamePlan::Rename { old, new } => {
            let old_path = workspace.join(format!("{old}.txt"));
            let new_path = workspace.join(format!("{new}.txt"));

            if tokio::fs::metadata(&old_path).await.is_err() {
                debug!(
                    "plot_sync: source {:?} does not exist, rebaseline",
                    old_path
                );
                state.lock().baseline = current;
                return;
            }
            let same_path_ci = old_path
                .to_string_lossy()
                .eq_ignore_ascii_case(&new_path.to_string_lossy());
            if !same_path_ci && tokio::fs::metadata(&new_path).await.is_ok() {
                warn!("plot_sync: target {:?} already exists, skip", new_path);
                client
                    .show_message(
                        MessageType::WARNING,
                        format!("{new}.txt は既に存在するため、リネームをスキップしました"),
                    )
                    .await;
                state.lock().baseline = current;
                return;
            }

            let renamed =
                rename_chapter_file(client, text, &old_path, &new_path, supports_rename_op).await;

            if renamed {
                state.lock().note_self_rename(old_path, new_path);
            }
            // 成功・失敗いずれでも baseline は現状(current)へ合わせる。失敗時に古い baseline
            // のまま残すと、次の idle で再び同じ Rename を試みて延々と警告が出るため。
            state.lock().baseline = current;
        }
    }
}

/// 実際のリネームを実行する。`client.apply_edit` に `ResourceOp::Rename` を積むのを第一手段とし、
/// クライアントが `resource_operations` に `Rename` を含まないと分かっている場合のみ
/// `tokio::fs::rename` にフォールバックする。対象が開いたままフォールバックした場合は警告を出す。
/// 戻り値は「(サーバーの視点で)リネームが実行された」かどうか。
#[instrument(skip(client, text))]
async fn rename_chapter_file(
    client: &Client,
    text: &DashMap<String, Vec<LineData>>,
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    supports_rename_op: bool,
) -> bool {
    use tower_lsp_server::UriExt;
    let Some(old_uri) = Uri::from_file_path(old_path) else {
        error!("plot_sync: failed to build uri for {:?}", old_path);
        return false;
    };
    let Some(new_uri) = Uri::from_file_path(new_path) else {
        error!("plot_sync: failed to build uri for {:?}", new_path);
        return false;
    };
    let was_open = text.contains_key(old_uri.as_str());

    if supports_rename_op {
        let edit = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri,
                    new_uri,
                    options: Some(RenameFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(false),
                    }),
                    annotation_id: None,
                })),
            ])),
            ..Default::default()
        };
        match client.apply_edit(edit).await {
            Ok(resp) if resp.applied => {
                debug!(
                    "plot_sync: renamed via apply_edit {:?} -> {:?}",
                    old_path, new_path
                );
                return true;
            }
            Ok(resp) => {
                warn!(
                    "plot_sync: apply_edit rejected rename ({:?}), not falling back",
                    resp.failure_reason
                );
                return false;
            }
            Err(err) => {
                warn!(
                    "plot_sync: apply_edit request failed: {:?}, falling back to fs::rename",
                    err
                );
            }
        }
    }

    match tokio::fs::rename(old_path, new_path).await {
        Ok(()) => {
            if was_open {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "{} を開いたままリネームしました。タブを開き直してください",
                            new_path.file_name().and_then(|n| n.to_str()).unwrap_or("")
                        ),
                    )
                    .await;
            }
            true
        }
        Err(err) => {
            error!("plot_sync: fs::rename failed: {:?}", err);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- diff_chapter_names ----

    #[test]
    fn test_diff_no_change_is_noop() {
        let b = names(&["第1章", "第2章"]);
        assert_eq!(diff_chapter_names(&b, &b.clone()), RenamePlan::Noop);
    }

    #[test]
    fn test_diff_single_rename() {
        let b = names(&["第1章", "第2章"]);
        let c = names(&["序章", "第2章"]);
        assert_eq!(
            diff_chapter_names(&b, &c),
            RenamePlan::Rename {
                old: "第1章".to_string(),
                new: "序章".to_string()
            }
        );
    }

    #[test]
    fn test_diff_chapter_added_is_rebaseline() {
        let b = names(&["第1章"]);
        let c = names(&["第1章", "第2章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_chapter_removed_is_rebaseline() {
        let b = names(&["第1章", "第2章"]);
        let c = names(&["第1章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_two_chapters_changed_is_rebaseline() {
        let b = names(&["第1章", "第2章"]);
        let c = names(&["序章", "終章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_reorder_is_rebaseline() {
        let b = names(&["第1章", "第2章"]);
        let c = names(&["第2章", "第1章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_empty_new_name_is_rebaseline() {
        let b = names(&["第1章"]);
        let c = names(&[""]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_duplicate_in_baseline_is_rebaseline() {
        let b = names(&["第1章", "第1章"]);
        let c = names(&["第1章", "序章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    #[test]
    fn test_diff_duplicate_in_current_is_rebaseline() {
        let b = names(&["第1章", "第2章"]);
        let c = names(&["第1章", "第1章"]);
        assert_eq!(diff_chapter_names(&b, &c), RenamePlan::Rebaseline);
    }

    // ---- is_valid_chapter_file_name ----

    #[test]
    fn test_valid_japanese_name() {
        assert!(is_valid_chapter_file_name("第一章 決戦"));
    }

    #[test]
    fn test_invalid_path_separator() {
        assert!(!is_valid_chapter_file_name("a/b"));
        assert!(!is_valid_chapter_file_name("a\\b"));
    }

    #[test]
    fn test_invalid_windows_forbidden_chars() {
        for c in ['<', '>', ':', '"', '|', '?', '*'] {
            assert!(!is_valid_chapter_file_name(&format!("章{c}名")), "char={c}");
        }
    }

    #[test]
    fn test_invalid_reserved_names() {
        assert!(!is_valid_chapter_file_name("CON"));
        assert!(!is_valid_chapter_file_name("con"));
        assert!(!is_valid_chapter_file_name("con.txt"));
        assert!(!is_valid_chapter_file_name("COM1"));
    }

    #[test]
    fn test_invalid_trailing_dot() {
        assert!(!is_valid_chapter_file_name("第1章."));
    }

    #[test]
    fn test_invalid_trailing_whitespace() {
        assert!(!is_valid_chapter_file_name("第1章 "));
        assert!(!is_valid_chapter_file_name(" 第1章"));
    }

    #[test]
    fn test_invalid_empty_name() {
        assert!(!is_valid_chapter_file_name(""));
    }
}
