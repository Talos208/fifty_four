//! ACP エージェント(`--acp`)と LSP サーバの間で「いま話している内容」を受け渡す。
//!
//! Zed は LSP サーバ(拡張経由)と ACP エージェント(`agent_servers` 設定経由)を
//! それぞれ別プロセスとして起動するため、両者はメモリを共有できない。そこで
//! ワークスペース直下の `.fifty_four/chat_context.md` を唯一の受け渡し点とする。
//! `plot.md` やキャラクター設定を呼ばれるたびディスクから読み直す
//! [`crate::tools`] と同じ方式で、プロセス間の同期機構を持ち込まずに済ませる。
//!
//! - 書き手は ACP エージェント(1ターンごとに要約を上書き)
//! - 読み手は LSP サーバ(補完・code action のプロンプト組み立て時)
//!
//! 書き手の ACP エージェントは debug ビルド限定だが、読み手は release でも動く。
//! release バイナリでは「要約を書く者が居ないので常に要約なし」として素通りする。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// ワークスペース直下に掘る作業ディレクトリ名。
const DIR_NAME: &str = ".fifty_four";

/// チャット要約の保存ファイル名。
const FILE_NAME: &str = "chat_context.md";

/// プロンプトへ埋め込む要約の既定上限(文字数)。
///
/// 補完は速度優先のターンなので、会話が長くなっても本文(`{{TEXT}}`)を
/// 押しのけない程度に抑える。
pub(crate) const DEFAULT_MAX_CHARS: usize = 1200;

/// 要約を有効とみなす既定の鮮度(秒)。
///
/// 昨日の会話が今日の補完に混ざるのを防ぐ。既定は 12 時間。
pub(crate) const DEFAULT_TTL_SECS: u64 = 12 * 60 * 60;

/// 受け渡しファイルのパスを返す。
pub(crate) fn digest_path(root: &Path) -> PathBuf {
    root.join(DIR_NAME).join(FILE_NAME)
}

/// 要約を原子的に書き出す。
///
/// 一時ファイルへ書いてから `rename` する。読み手(LSP)は補完のたびに
/// 無条件でこのファイルを読むため、書きかけの内容を読ませないことが重要。
/// 同一ディレクトリ内の `rename` は同一ファイルシステム上なので原子的に行われる。
///
/// 呼び手の ACP エージェントが debug 限定なので、この関数も同じ範囲に合わせてある
/// (release で未使用の dead code にしないため)。`test` を含めるのは
/// `cargo test --release` でもテストが通るようにするため。
#[cfg(any(debug_assertions, test))]
pub(crate) fn write_digest(root: &Path, digest: &str) -> std::io::Result<()> {
    let dir = root.join(DIR_NAME);
    std::fs::create_dir_all(&dir)?;

    // 並列書き込み・複数プロセスの衝突を避けるため一時ファイル名に PID+連番を含める
    // (`Highlighter::rebuild_user_dictionary` と同じ方式)。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        FILE_NAME,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));

    std::fs::write(&tmp, digest)?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(FILE_NAME)) {
        // rename に失敗したら一時ファイルを残さない(次回以降のゴミを作らない)。
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 要約を読み出す。プロンプトへ埋め込めない状態なら `None`。
///
/// `None` を返すのは次の場合:
/// - ファイルが無い(まだ一度もチャットしていない)
/// - 最終更新が `ttl` より古い(会話が古すぎて今書いている場面と関係ない)
/// - 中身が空白のみ
///
/// `max_chars` を超える場合は**古い側(先頭)から**切り落とす。要約は
/// 新しい話題ほど後ろに来るため、末尾を残す方が「いま何を書こうとしているか」に近い。
pub(crate) fn read_digest(root: &Path, max_chars: usize, ttl: Duration) -> Option<String> {
    let path = digest_path(root);

    // 鮮度の判定。mtime が取れない環境では鮮度チェックを諦めて内容を採用する
    // (取れないことを理由に機能ごと無効化はしない)。
    if let Ok(meta) = std::fs::metadata(&path)
        && let Ok(modified) = meta.modified()
        && let Ok(age) = SystemTime::now().duration_since(modified)
        && age > ttl
    {
        return None;
    }

    let body = std::fs::read_to_string(&path).ok()?;
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(tail_chars(body, max_chars))
}

/// 末尾から `max_chars` 文字を残して返す。`max_chars` 以下ならそのまま。
///
/// バイト単位ではなく文字単位で切るため、日本語でも文字境界を壊さない。
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

    /// テストごとに独立したディレクトリを用意する(既存テストと同じ temp_dir 方式)。
    fn fresh_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff_chat_context_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_write_then_read_roundtrip() {
        let root = fresh_root("roundtrip");
        write_digest(&root, "いまは第3章の別れの場面を書いている。").unwrap();

        let got = read_digest(&root, DEFAULT_MAX_CHARS, Duration::from_secs(3600)).unwrap();
        assert_eq!(got, "いまは第3章の別れの場面を書いている。");
    }

    #[test]
    fn test_read_missing_file_returns_none() {
        let root = fresh_root("missing");
        assert!(read_digest(&root, DEFAULT_MAX_CHARS, Duration::from_secs(3600)).is_none());
    }

    #[test]
    fn test_read_blank_content_returns_none() {
        let root = fresh_root("blank");
        write_digest(&root, "   \n\n  ").unwrap();
        assert!(read_digest(&root, DEFAULT_MAX_CHARS, Duration::from_secs(3600)).is_none());
    }

    #[test]
    fn test_read_expired_returns_none() {
        let root = fresh_root("expired");
        write_digest(&root, "古い会話").unwrap();
        // TTL 0 なら書いた直後でも「古い」と判定される
        assert!(read_digest(&root, DEFAULT_MAX_CHARS, Duration::from_secs(0)).is_none());
    }

    #[test]
    fn test_read_trims_from_the_head() {
        let root = fresh_root("trim");
        write_digest(&root, "あいうえおかきくけこ").unwrap();

        // 新しい話題(末尾)が残ること
        let got = read_digest(&root, 3, Duration::from_secs(3600)).unwrap();
        assert_eq!(got, "くけこ");
    }

    #[test]
    fn test_write_overwrites_previous_digest() {
        let root = fresh_root("overwrite");
        write_digest(&root, "ふるい").unwrap();
        write_digest(&root, "あたらしい").unwrap();

        let got = read_digest(&root, DEFAULT_MAX_CHARS, Duration::from_secs(3600)).unwrap();
        assert_eq!(got, "あたらしい");

        // 一時ファイルが残っていないこと
        let leftovers: Vec<_> = std::fs::read_dir(root.join(DIR_NAME))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "一時ファイルが残っている");
    }

    #[test]
    fn test_tail_chars_keeps_multibyte_boundaries() {
        assert_eq!(tail_chars("あいう", 10), "あいう");
        assert_eq!(tail_chars("あいう", 0), "");
        assert_eq!(tail_chars("abcあいう", 4), "cあいう");
    }
}
