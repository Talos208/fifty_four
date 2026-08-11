//! ACP セッションの会話ログを、プロセス再起動をまたいで永続化する。
//!
//! [`crate::acp`] の `Session::turns` はプロセスのメモリ上にしか無く、
//! `fifty_four_lsp --acp` が再起動すると失われる。ACP の仕様は `session/load` で
//! 会話全体を `session/update` 通知としてリプレイすることを要求しているため
//! (応答を返す前に MUST)、リプレイ元になるデータをどこかへ残しておく必要がある。
//!
//! `claude` CLI 自身もディスクへ会話を永続化しているが、保存先パス(cwdを
//! ハッシュ化したディレクトリ名)は非公開仕様でバージョン間の互換性が保証されない。
//! そこで `chat_context.rs` と同じ発想で、こちらの管理下にある
//! `.fifty_four/sessions/<session_id>.jsonl.gz` へ自前で逐次追記する。
//!
//! # フォーマット
//!
//! 1ターン = 1行の JSON([`crate::acp::ChatTurn`] をそのままシリアライズしたもの)を、
//! gzip の「独立したメンバーを単純に連結してよい」という性質を使って1メンバーずつ
//! 追記する。これにより `chat_context::write_digest` と同じ append-only な書き方を、
//! 圧縮ありのまま維持できる(読み直し・再圧縮が不要)。読み出し側は
//! [`flate2::read::MultiGzDecoder`] を使うことで、連結された複数メンバーを
//! 意識せず1本のストリームとして読める。
//!
//! 保持期間の制限(TTL)は設けていない。ログは増え続けるので、容量が気になる場合は
//! [`crate::chat_context::DEFAULT_TTL_SECS`] と同様の仕組みをここへ足す余地がある。

use crate::acp::ChatTurn;
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use log::warn;
use tracing::instrument;

/// セッションログを置くサブディレクトリ名(`.fifty_four/` 直下)。
const SESSIONS_DIR: &str = "sessions";

/// セッションログのファイルパスを返す。
#[instrument]
fn log_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(crate::chat_context::DIR_NAME)
        .join(SESSIONS_DIR)
        .join(format!("{}.jsonl.gz", session_id))
}

/// 1ターンを末尾へ追記する。
///
/// セッションIDごとに `session/prompt` は直列に処理されるため
/// (`crate::acp` の `sessions` ロック参照)、書き込みの競合は起きない。
/// 失敗しても呼び出し側は `warn!` に落として会話自体は止めない
/// (`chat_context::write_digest` の失敗時と同じ扱い)。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[instrument]
pub(crate) fn append_turn(root: &Path, session_id: &str, turn: &ChatTurn) -> std::io::Result<()> {
    let dir = root.join(crate::chat_context::DIR_NAME).join(SESSIONS_DIR);
    std::fs::create_dir_all(&dir)?;

    let line = serde_json::to_string(turn)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(line.as_bytes())?;
    encoder.write_all(b"\n")?;
    let member = encoder.finish()?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(root, session_id))?;
    file.write_all(&member)
}

/// 過去の全ターンを読み出す。
///
/// ログが無い(初回・旧セッション・削除済み)場合は空の `Vec` を返す。
/// 壊れた行が混ざっていても、その行だけ読み飛ばして残りは返す
/// (`parse_rate_limit_event` と同じ「取れなければ諦めて続行する」方針)。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[instrument]
pub(crate) fn read_turns(root: &Path, session_id: &str) -> Vec<ChatTurn> {
    let path = log_path(root, session_id);
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };

    let mut decoder = MultiGzDecoder::new(file);
    let mut content = String::new();
    if let Err(e) = decoder.read_to_string(&mut content) {
        warn!(
            "acp: セッションログの展開に失敗しました({}): {}",
            path.display(),
            e
        );
        return Vec::new();
    }

    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<ChatTurn>(line) {
            Ok(turn) => Some(turn),
            Err(e) => {
                warn!("acp: セッションログの1行を読み飛ばしました: {}", e);
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::Speaker;

    fn fresh_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff_session_log_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn turn(speaker: Speaker, text: &str) -> ChatTurn {
        ChatTurn {
            speaker,
            text: text.to_string(),
        }
    }

    #[test]
    fn test_append_then_read_roundtrip_preserves_order() {
        let root = fresh_root("roundtrip");
        append_turn(&root, "s1", &turn(Speaker::Author, "1ターン目")).unwrap();
        append_turn(&root, "s1", &turn(Speaker::Agent, "応答1")).unwrap();
        append_turn(&root, "s1", &turn(Speaker::Author, "2ターン目")).unwrap();

        let got = read_turns(&root, "s1");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].speaker, Speaker::Author);
        assert_eq!(got[0].text, "1ターン目");
        assert_eq!(got[1].speaker, Speaker::Agent);
        assert_eq!(got[1].text, "応答1");
        assert_eq!(got[2].text, "2ターン目");
    }

    #[test]
    fn test_read_missing_file_returns_empty() {
        let root = fresh_root("missing");
        assert!(read_turns(&root, "no-such-session").is_empty());
    }

    #[test]
    fn test_read_skips_corrupted_line_but_keeps_others() {
        let root = fresh_root("corrupted");
        append_turn(&root, "s1", &turn(Speaker::Author, "壊れていない発話1")).unwrap();

        // 壊れた1行(有効なgzipメンバーだが中身がJSONとして不正)を手動で追記する。
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"not valid json\n").unwrap();
        let member = encoder.finish().unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(log_path(&root, "s1"))
            .unwrap();
        file.write_all(&member).unwrap();

        append_turn(&root, "s1", &turn(Speaker::Agent, "壊れていない発話2")).unwrap();

        let got = read_turns(&root, "s1");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "壊れていない発話1");
        assert_eq!(got[1].text, "壊れていない発話2");
    }

    #[test]
    fn test_speaker_serde_uses_snake_case() {
        let json = serde_json::to_string(&Speaker::Author).unwrap();
        assert_eq!(json, "\"author\"");
        let json = serde_json::to_string(&Speaker::Agent).unwrap();
        assert_eq!(json, "\"agent\"");
    }

    #[test]
    fn test_log_uses_gzip_multi_member_and_is_smaller_than_plain_text_for_repetitive_text() {
        let root = fresh_root("compressed");
        let long_text = "同じ話題について長めに話す。".repeat(50);
        for _ in 0..5 {
            append_turn(&root, "s1", &turn(Speaker::Author, &long_text)).unwrap();
        }
        let compressed_len = std::fs::metadata(log_path(&root, "s1")).unwrap().len() as usize;
        let plain_len = long_text.len() * 5;
        assert!(
            compressed_len < plain_len,
            "compressed({}) should be smaller than plain({})",
            compressed_len,
            plain_len
        );
        assert_eq!(read_turns(&root, "s1").len(), 5);
    }
}
