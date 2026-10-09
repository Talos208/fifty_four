//! ACP エージェント(`--acp`)と LSP サーバの間で「いま話している内容」を受け渡す。
//! 別プロセス同士のため、ワークスペース直下の `.fifty_four/chat_context.md` を
//! 唯一の受け渡し点とする(書き手は ACP、読み手は LSP の補完・code action)。
//!
//! `chat_context.owner` によるセッション単位の所有権管理の背景は `docs/acp-agent.md`
//! の「セッションの再開」参照。

use std::path::{Path, PathBuf};
use tracing::instrument;

/// ワークスペース直下に掘る作業ディレクトリ名。
///
/// [`crate::session_log`] もセッションログの保存先(`<DIR_NAME>/sessions/`)として
/// このディレクトリを共有する。
pub(crate) const DIR_NAME: &str = ".fifty_four";

/// チャット要約の保存ファイル名。
const FILE_NAME: &str = "chat_context.md";

/// 要約の所有者(セッションID)を記録するファイル名。
const OWNER_FILE_NAME: &str = "chat_context.owner";

/// プロンプトへ埋め込む要約の既定上限(文字数)。
///
/// 補完は速度優先のターンなので、会話が長くなっても本文(`{{TEXT}}`)を
/// 押しのけない程度に抑える。
pub(crate) const DEFAULT_MAX_CHARS: usize = 1200;

/// 受け渡しファイルのパスを返す。
pub(crate) fn digest_path(root: &Path) -> PathBuf {
    root.join(DIR_NAME).join(FILE_NAME)
}

/// 所有者マーカーのパスを返す。
fn owner_path(root: &Path) -> PathBuf {
    root.join(DIR_NAME).join(OWNER_FILE_NAME)
}

/// `.fifty_four/` 直下へ1ファイルを原子的に書き出す。
///
/// 一時ファイルへ書いてから `rename` する。読み手(LSP)は補完のたびに
/// 無条件でこのファイルを読むため、書きかけの内容を読ませないことが重要。
/// 同一ディレクトリ内の `rename` は同一ファイルシステム上なので原子的に行われる。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[instrument]
fn write_atomic(dir: &Path, file_name: &str, content: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    // 並列書き込み・複数プロセスの衝突を避けるため一時ファイル名に PID+連番を含める
    // (`Highlighter::rebuild_user_dictionary` と同じ方式)。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        file_name,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));

    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(file_name)) {
        // rename に失敗したら一時ファイルを残さない(次回以降のゴミを作らない)。
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 要約とその所有者(セッションID)を原子的に書き出す。
///
/// 書き手は `writing_agent`(debugビルド限定)のみなので、releaseビルドでは未使用になる。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
#[instrument]
pub(crate) fn write_digest(root: &Path, digest: &str, session_id: &str) -> std::io::Result<()> {
    let dir = root.join(DIR_NAME);
    write_atomic(&dir, FILE_NAME, digest)?;
    write_atomic(&dir, OWNER_FILE_NAME, session_id)
}

/// 要約を読み出す。プロンプトへ埋め込めない状態(ファイル無し・空白のみ)なら `None`。
///
/// `max_chars` 超過時は**古い側(先頭)から**切り落とす。新しい話題ほど後ろに来るため、
/// 末尾を残す方が「いま何を書こうとしているか」に近い。鮮度チェック(TTL)廃止の経緯は
/// `docs/acp-agent.md` の「セッションの再開」参照。
#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
#[instrument]
pub(crate) fn read_digest(root: &Path, max_chars: usize) -> Option<String> {
    let path = digest_path(root);
    let body = std::fs::read_to_string(&path).ok()?;
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(tail_chars(body, max_chars))
}

/// 要約を現在所有しているセッションIDを読む。無ければ `None`。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
#[instrument]
pub(crate) fn owner(root: &Path) -> Option<String> {
    let body = std::fs::read_to_string(owner_path(root)).ok()?;
    let body = body.trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// 要約と所有者マーカーの両方を消す。
///
/// 「新しい会話なので前の要約を引き継がない」(`session/new`)、
/// 「復元先セッションに要約の材料が無いので古い所有者のものを残さない」
/// (`session/load`)の両方で使う。ファイルが元から無い場合はエラーにしない。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
#[instrument]
pub(crate) fn clear(root: &Path) -> std::io::Result<()> {
    for path in [digest_path(root), owner_path(root)] {
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e);
        }
    }
    Ok(())
}

/// 末尾から `max_chars` 文字を残して返す。`max_chars` 以下ならそのまま。
///
/// バイト単位ではなく文字単位で切るため、日本語でも文字境界を壊さない。
#[instrument]
fn tail_chars(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    s.chars().skip(total - max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストごとに独立したディレクトリを用意する(`TempDir` は Drop で削除される)。
    fn fresh_root(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("ff_chat_context_{name}_"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn test_write_then_read_roundtrip() {
        let _tmp = fresh_root("roundtrip");
        let root = _tmp.path();
        write_digest(&root, "いまは第3章の別れの場面を書いている。", "s1").unwrap();

        let got = read_digest(&root, DEFAULT_MAX_CHARS).unwrap();
        assert_eq!(got, "いまは第3章の別れの場面を書いている。");
    }

    #[test]
    fn test_read_missing_file_returns_none() {
        let _tmp = fresh_root("missing");
        let root = _tmp.path();
        assert!(read_digest(&root, DEFAULT_MAX_CHARS).is_none());
    }

    #[test]
    fn test_read_blank_content_returns_none() {
        let _tmp = fresh_root("blank");
        let root = _tmp.path();
        write_digest(&root, "   \n\n  ", "s1").unwrap();
        assert!(read_digest(&root, DEFAULT_MAX_CHARS).is_none());
    }

    #[test]
    fn test_read_trims_from_the_head() {
        let _tmp = fresh_root("trim");
        let root = _tmp.path();
        write_digest(&root, "あいうえおかきくけこ", "s1").unwrap();

        // 新しい話題(末尾)が残ること
        let got = read_digest(&root, 3).unwrap();
        assert_eq!(got, "くけこ");
    }

    #[test]
    fn test_write_overwrites_previous_digest() {
        let _tmp = fresh_root("overwrite");
        let root = _tmp.path();
        write_digest(&root, "ふるい", "s1").unwrap();
        write_digest(&root, "あたらしい", "s2").unwrap();

        let got = read_digest(&root, DEFAULT_MAX_CHARS).unwrap();
        assert_eq!(got, "あたらしい");
        assert_eq!(owner(&root).unwrap(), "s2");

        // 一時ファイルが残っていないこと
        let leftovers: Vec<_> = std::fs::read_dir(root.join(DIR_NAME))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "一時ファイルが残っている");
    }

    #[test]
    fn test_owner_roundtrip() {
        let _tmp = fresh_root("owner_roundtrip");
        let root = _tmp.path();
        assert!(owner(&root).is_none());
        write_digest(&root, "会話の要約", "session-abc").unwrap();
        assert_eq!(owner(&root).unwrap(), "session-abc");
    }

    #[test]
    fn test_clear_removes_digest_and_owner() {
        let _tmp = fresh_root("clear");
        let root = _tmp.path();
        write_digest(&root, "消えるはずの要約", "s1").unwrap();
        assert!(read_digest(&root, DEFAULT_MAX_CHARS).is_some());
        assert!(owner(&root).is_some());

        clear(&root).unwrap();

        assert!(read_digest(&root, DEFAULT_MAX_CHARS).is_none());
        assert!(owner(&root).is_none());
    }

    #[test]
    fn test_clear_on_missing_files_is_not_an_error() {
        let _tmp = fresh_root("clear_missing");
        let root = _tmp.path();
        clear(&root).unwrap();
    }

    #[test]
    fn test_tail_chars_keeps_multibyte_boundaries() {
        assert_eq!(tail_chars("あいう", 10), "あいう");
        assert_eq!(tail_chars("あいう", 0), "");
        assert_eq!(tail_chars("abcあいう", 4), "cあいう");
    }
}
