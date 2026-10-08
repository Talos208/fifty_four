//! デバッグビルド専用の SQLite 記録層。
//!
//! `main.rs` から切り出したモジュール。completion 候補やキャラクター設定更新の
//! 履歴を記録し、後から `db/fifty_four.db` を見て挙動を追跡できるようにする。
//!
//! release ビルドでは全メソッドが no-op のスタブに切り替わる(`#[cfg(not(debug_assertions))]`)。
//! 記録先のパス解決もこのモジュール内に閉じ込めてあるため、release では
//! パスの組み立ても `db/` の作成も一切行わない(`assets.rs` と同じ方針)。

#[allow(unused_imports)]
use indoc::indoc;
#[allow(unused_imports)]
use log::debug;
#[cfg(debug_assertions)]
use std::path::{Path, PathBuf};
use tower_lsp_server::lsp_types::TextDocumentContentChangeEvent;

#[cfg(debug_assertions)]
mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

/// 直近のcompletion候補を記録する構造体（デバッグビルドのみ）
#[derive(Debug, Clone)]
pub(crate) struct PendingCandidate {
    #[cfg(debug_assertions)]
    db_id: i64,
    #[cfg(debug_assertions)]
    candidate: String,
}

/// `quality_findings` へ書く1件分。決定的ルール(`source = "rule"`)も LLM 診断(`"llm"`)も同じ形。
/// release ビルドでも呼び出し側が同じコードで組み立てられるよう、cfg の外に置いている。
#[derive(Debug, Clone)]
pub(crate) struct QualityFindingRecord {
    pub source: &'static str,
    pub rule_code: String,
    pub severity: &'static str,
    pub line: usize,
    pub excerpt: String,
    pub message: String,
    pub review_id: Option<i64>,
}

/// デバッグビルド専用のDB操作をカプセル化する構造体
#[cfg(debug_assertions)]
#[derive(Debug)]
pub(crate) struct FlightRecorder {
    conn: parking_lot::Mutex<rusqlite::Connection>,
    pending_completions: parking_lot::Mutex<Option<(String, Vec<PendingCandidate>)>>,
}

#[cfg(debug_assertions)]
impl FlightRecorder {
    /// 既定の場所(`<実行ファイルの隣>/db/fifty_four.db`)を開く。
    ///
    /// db/ を実行ファイル自身の隣に置くのは、コピー先のフォルダでもそのまま動くようにするため。
    /// ビルド時に固定される `CARGO_MANIFEST_DIR` ではなく実行時の `current_exe` を基準にする。
    /// `current_exe` が取れない場合のみ `CARGO_MANIFEST_DIR` の親へフォールバックする。
    pub(crate) fn open_default() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("CARGO_MANIFEST_DIR has no parent")
                    .to_path_buf()
            });

        let db_dir = exe_dir.join("db");
        std::fs::create_dir_all(&db_dir)
            .unwrap_or_else(|e| panic!("Failed to create db directory {:?}: {}", db_dir, e));

        Self::new(&db_dir.join("fifty_four.db"))
    }

    pub(crate) fn new(path: &PathBuf) -> Self {
        // マイグレーションも済ませてしまう
        let mut c = rusqlite::Connection::open(path).expect("Fail to open database");
        match migrations::migrations::runner().run(&mut c) {
            Ok(_) => {}
            Err(e) => {
                panic!("Fail to migrate: {:?}", e);
            }
        }

        Self {
            conn: parking_lot::Mutex::new(c),
            pending_completions: parking_lot::Mutex::new(None),
        }
    }

    /// INSERT INTO completions ... RETURNING id。失敗時は 0 を返す。
    pub(crate) fn record_completion(
        &self,
        uri: &str,
        line_no: usize,
        offset: usize,
        model: &str,
        prompt: &str,
    ) -> u32 {
        use std::time::Duration;

        if let Some(db) = self.conn.try_lock_for(Duration::from_secs(1)) {
            db.query_row(
                indoc!(
                    "INSERT INTO completions
                    (document_uri, cursor_line, cursor_character, model_name, prompt)
                    VALUES (?,?,?,?,?) RETURNING id;"
                ),
                rusqlite::params![
                    uri,
                    line_no.to_string().as_str(),
                    offset.to_string().as_str(),
                    model,
                    prompt,
                ],
                |row| row.get(0),
            )
            .unwrap_or(0)
        } else {
            0
        }
    }

    /// INSERT INTO completion_candidates ... RETURNING id。成功時に pending に push。
    pub(crate) fn record_candidate(
        &self,
        completion_id: u32,
        rank: usize,
        confidence: Option<f64>,
        candidate_text: &str,
        display_text: &str,
        pending: &mut Vec<PendingCandidate>,
    ) {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            match db.query_row(
                indoc!(
                    "INSERT INTO completion_candidates
                    (completion_id, rank, confidence, candidate)
                    VALUES (?,?,?,?) RETURNING id;"
                ),
                rusqlite::params![completion_id, rank as i64, confidence, candidate_text],
                |row| row.get::<_, i64>(0),
            ) {
                Ok(id) => pending.push(PendingCandidate {
                    db_id: id,
                    candidate: display_text.to_string(),
                }),
                Err(err) => debug!("Failed to insert completion_candidate: {}", err),
            }
        }
    }

    pub(crate) fn set_completions(&self, uri: String, candidates: Vec<PendingCandidate>) {
        use std::time::Duration;

        if let Some(mut cmp) = self
            .pending_completions
            .try_lock_for(Duration::from_secs(1))
        {
            *cmp = Some((uri, candidates));
        }
    }

    pub(crate) fn mark_selected_completion(
        &self,
        uri: &str,
        content_changes: &[TextDocumentContentChangeEvent],
    ) {
        let (pending_uri, candidates) = {
            let Some(cmp) = self
                .pending_completions
                .try_lock_for(std::time::Duration::from_secs(1))
            else {
                return;
            };

            cmp.clone().unwrap_or(("".to_string(), vec![]))
        };

        if pending_uri != uri {
            return;
        }

        for change in content_changes {
            if let Some(c) = candidates.iter().find(|c| c.candidate == change.text) {
                use std::time::Duration;

                let Some(db) = self.conn.try_lock_for(Duration::from_secs(1)) else {
                    return;
                };
                if let Err(e) = db.execute(
                    "UPDATE completion_candidates SET selected = true WHERE id = ?;",
                    rusqlite::params![c.db_id],
                ) {
                    debug!("Failed to update completion_candidates: {}", e);
                }

                break;
            }
        }
    }

    /// INSERT INTO code_actions ... RETURNING id。失敗時は -1 を返す。
    ///
    /// `record_completion` と同じく detached task から呼ばれ、補完の記録と
    /// 並走しうるため `try_lock_for` で待つ(`record_character_*` の無制限 `lock()` とは違う)。
    pub(crate) fn record_code_action(
        &self,
        uri: &str,
        mode: &str,
        target_text: &str,
        model: &str,
        prompt: &str,
    ) -> i64 {
        use std::time::Duration;

        if let Some(db) = self.conn.try_lock_for(Duration::from_secs(1)) {
            db.query_row(
                indoc!(
                    "INSERT INTO code_actions
                    (document_uri, mode, target_text, model_name, prompt)
                    VALUES (?,?,?,?,?) RETURNING id;"
                ),
                rusqlite::params![uri, mode, target_text, model, prompt],
                |row| row.get(0),
            )
            .unwrap_or(-1)
        } else {
            -1
        }
    }

    /// code_actions.response へ LLM の生応答(パース前)を書き込む。
    ///
    /// `max_tokens` の見積もりが妥当かを後から検証するため
    /// (`length(response)` が上限に張り付いていないかを見る)。
    pub(crate) fn record_code_action_response(&self, code_action_id: i64, response: &str) {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            if let Err(e) = db.execute(
                "UPDATE code_actions SET response = ? WHERE id = ?;",
                rusqlite::params![response, code_action_id],
            ) {
                debug!("record_code_action_response failed: {}", e);
            }
        }
    }

    /// INSERT INTO code_action_candidates。
    pub(crate) fn record_code_action_candidate(
        &self,
        code_action_id: i64,
        rank: usize,
        candidate: &str,
        confidence: Option<f64>,
    ) {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            if let Err(e) = db.execute(
                indoc!(
                    "INSERT INTO code_action_candidates
                    (code_action_id, rank, candidate, confidence)
                    VALUES (?,?,?,?);"
                ),
                rusqlite::params![code_action_id, rank as i64, candidate, confidence],
            ) {
                debug!("record_code_action_candidate failed: {}", e);
            }
        }
    }

    pub(crate) fn record_character_update(&self, uri: &str, model: &str, prompt: &str) -> i64 {
        let db = self.conn.lock();
        db.query_row(
            "INSERT INTO character_updates (document_uri, model_name, prompt) VALUES (?,?,?) RETURNING id;",
            rusqlite::params![uri, model, prompt],
            |row| row.get(0),
        ).unwrap_or(-1)
    }

    pub(crate) fn record_character_response(&self, update_id: i64, response: &str) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "UPDATE character_updates SET response = ? WHERE id = ?;",
            rusqlite::params![response, update_id],
        ) {
            debug!("record_character_response failed: {}", e);
        }
    }

    pub(crate) fn record_character_section(
        &self,
        update_id: i64,
        name: &str,
        attr: &str,
        old_text: Option<&str>,
        new_text: &str,
        applied: bool,
        skip_reason: Option<&str>,
    ) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "INSERT INTO character_update_sections (update_id, character_name, attribute, old_text, new_text, applied, skip_reason) VALUES (?,?,?,?,?,?,?);",
            rusqlite::params![update_id, name, attr, old_text, new_text, applied, skip_reason],
        ) {
            debug!("record_character_section failed: {}", e);
        }
    }

    pub(crate) fn complete_character_update(&self, update_id: i64) {
        let db = self.conn.lock();
        if let Err(e) = db.execute(
            "UPDATE character_updates SET completed_at = datetime('now', 'subsec') WHERE id = ?;",
            rusqlite::params![update_id],
        ) {
            debug!("complete_character_update failed: {}", e);
        }
    }

    /// INSERT INTO quality_reviews ... RETURNING id。失敗時は -1 を返す。
    /// detached task から呼ばれ補完の記録と並走しうるため `try_lock_for` で待つ。
    pub(crate) fn record_quality_review(&self, uri: &str, model: &str, prompt: &str) -> i64 {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            db.query_row(
                indoc!(
                    "INSERT INTO quality_reviews (document_uri, model_name, prompt)
                    VALUES (?,?,?) RETURNING id;"
                ),
                rusqlite::params![uri, model, prompt],
                |row| row.get(0),
            )
            .unwrap_or(-1)
        } else {
            -1
        }
    }

    /// quality_reviews.response へ LLM の生応答(パース前)を書き込む。
    pub(crate) fn record_quality_review_response(&self, review_id: i64, response: &str) {
        if let Some(db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) {
            if let Err(e) = db.execute(
                "UPDATE quality_reviews SET response = ? WHERE id = ?;",
                rusqlite::params![response, review_id],
            ) {
                debug!("record_quality_review_response failed: {}", e);
            }
        }
    }

    /// INSERT INTO quality_findings。1トランザクションにまとめる。
    pub(crate) fn record_quality_findings(&self, uri: &str, records: &[QualityFindingRecord]) {
        if records.is_empty() {
            return;
        }
        let Some(mut db) = self.conn.try_lock_for(std::time::Duration::from_secs(1)) else {
            return;
        };
        let result = (|| -> rusqlite::Result<()> {
            let tx = db.transaction()?;
            {
                let mut stmt = tx.prepare(indoc!(
                    "INSERT INTO quality_findings
                    (document_uri, source, rule_code, severity, line, excerpt, message, review_id)
                    VALUES (?,?,?,?,?,?,?,?);"
                ))?;
                for r in records {
                    stmt.execute(rusqlite::params![
                        uri,
                        r.source,
                        r.rule_code,
                        r.severity,
                        r.line as i64,
                        r.excerpt,
                        r.message,
                        r.review_id,
                    ])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = result {
            debug!("record_quality_findings failed: {}", e);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fifty_four_fr_test_{}_{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let _ = std::fs::remove_file(&path);
        path
    }

    fn rec(source: &'static str, code: &str) -> QualityFindingRecord {
        QualityFindingRecord {
            source,
            rule_code: code.to_string(),
            severity: "information",
            line: 3,
            excerpt: "抜粋".to_string(),
            message: "msg".to_string(),
            review_id: None,
        }
    }

    #[test]
    fn test_quality_migration_and_findings_roundtrip() {
        let fr = FlightRecorder::new(&temp_db("roundtrip"));
        let review_id = fr.record_quality_review("file:///a.txt", "model-x", "prompt");
        assert!(review_id > 0);
        fr.record_quality_review_response(review_id, "{\"findings\":[]}");

        let mut llm = rec("llm", "llm-review");
        llm.review_id = Some(review_id);
        fr.record_quality_findings("file:///a.txt", &[rec("rule", "no-chain"), llm]);

        let db = fr.conn.lock();
        let n: i64 = db
            .query_row("SELECT count(*) FROM quality_findings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let linked: i64 = db
            .query_row(
                "SELECT count(*) FROM quality_findings WHERE review_id = ?",
                [review_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked, 1);
        let resp: String = db
            .query_row(
                "SELECT response FROM quality_reviews WHERE id = ?",
                [review_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(resp.contains("findings"));
    }

    #[test]
    fn test_candidate_confidence_roundtrip() {
        let fr = FlightRecorder::new(&temp_db("confidence"));
        let cid = fr.record_completion("file:///a.txt", 0, 0, "model-x", "prompt");
        let mut pending = Vec::new();
        fr.record_candidate(cid, 0, Some(0.9), "高", "高", &mut pending);
        fr.record_candidate(cid, 1, None, "不明", "不明", &mut pending);
        let aid = fr.record_code_action("file:///a.txt", "rephrase", "対象", "model-x", "prompt");
        fr.record_code_action_candidate(aid, 0, "案", Some(0.7));

        let db = fr.conn.lock();
        let got: Vec<(i64, Option<f64>)> = db
            .prepare("SELECT rank, confidence FROM completion_candidates ORDER BY rank")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(got, vec![(0, Some(0.9)), (1, None)]);
        let ca: Option<f64> = db
            .query_row("SELECT confidence FROM code_action_candidates", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ca, Some(0.7));
    }

    #[test]
    fn test_record_quality_findings_empty_is_noop() {
        let fr = FlightRecorder::new(&temp_db("empty"));
        fr.record_quality_findings("file:///a.txt", &[]);
        let n: i64 = fr
            .conn
            .lock()
            .query_row("SELECT count(*) FROM quality_findings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[cfg(not(debug_assertions))]
#[derive(Debug)]
pub(crate) struct FlightRecorder {}

#[cfg(not(debug_assertions))]
impl FlightRecorder {
    /// release ビルドでは記録先を持たないため、パス解決もディレクトリ作成も行わない。
    pub(crate) fn open_default() -> Self {
        Self {}
    }

    pub(crate) fn record_completion(
        &self,
        _uri: &str,
        _line_no: usize,
        _offset: usize,
        _model: &str,
        _prompt: &str,
    ) -> u32 {
        0u32
    }

    pub(crate) fn record_candidate(
        &self,
        _completion_id: u32,
        _rank: usize,
        _confidence: Option<f64>,
        _candidate_text: &str,
        _display_text: &str,
        _pending: &mut Vec<PendingCandidate>,
    ) {
    }

    pub(crate) fn set_completions(&self, _uri: String, _candidates: Vec<PendingCandidate>) {}

    pub(crate) fn mark_selected_completion(
        &self,
        _uri: &str,
        _content_changes: &[TextDocumentContentChangeEvent],
    ) {
    }

    pub(crate) fn record_code_action(
        &self,
        _uri: &str,
        _mode: &str,
        _target_text: &str,
        _model: &str,
        _prompt: &str,
    ) -> i64 {
        -1
    }
    pub(crate) fn record_code_action_response(&self, _code_action_id: i64, _response: &str) {}
    pub(crate) fn record_code_action_candidate(
        &self,
        _code_action_id: i64,
        _rank: usize,
        _candidate: &str,
        _confidence: Option<f64>,
    ) {
    }

    pub(crate) fn record_character_update(&self, _uri: &str, _model: &str, _prompt: &str) -> i64 {
        -1
    }
    pub(crate) fn record_character_response(&self, _update_id: i64, _response: &str) {}
    pub(crate) fn record_character_section(
        &self,
        _update_id: i64,
        _name: &str,
        _attr: &str,
        _old_text: Option<&str>,
        _new_text: &str,
        _applied: bool,
        _skip_reason: Option<&str>,
    ) {
    }
    pub(crate) fn complete_character_update(&self, _update_id: i64) {}

    pub(crate) fn record_quality_review(&self, _uri: &str, _model: &str, _prompt: &str) -> i64 {
        -1
    }
    pub(crate) fn record_quality_review_response(&self, _review_id: i64, _response: &str) {}
    pub(crate) fn record_quality_findings(&self, _uri: &str, _records: &[QualityFindingRecord]) {}
}
