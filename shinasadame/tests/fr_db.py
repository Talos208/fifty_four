"""テスト用の FlightRecorder DB を作る。

`lsp/migrations/V*.sql` を本物のまま順に適用し、refinery が書く `refinery_schema_history` も再現する。
マイグレーションを複製せず実物を使うので、LSP 側のスキーマが変わればこのテストも追随して壊れる。
"""

import os
import re
import sqlite3
from pathlib import Path

# 既定はリポジトリの lsp/migrations。V5 を含まないブランチで確認したいときなどは
# FR_MIGRATIONS_DIR で差し替えられる。
MIGRATIONS = Path(
    os.environ.get("FR_MIGRATIONS_DIR")
    or Path(__file__).resolve().parents[2] / "lsp" / "migrations"
)


def _migrations(upto: int) -> list[tuple[int, str, Path]]:
    found = []
    for p in MIGRATIONS.glob("V*__*.sql"):
        m = re.match(r"V(\d+)__(.+)\.sql", p.name)
        if m and int(m.group(1)) <= upto:
            found.append((int(m.group(1)), m.group(2), p))
    return sorted(found)


def build(path: Path, version: int = 5, sample: bool = True) -> Path:
    conn = sqlite3.connect(path)
    conn.execute(
        "CREATE TABLE refinery_schema_history ("
        "version INT4 PRIMARY KEY, name VARCHAR(255), applied_on VARCHAR(255), checksum VARCHAR(255))"
    )
    for v, name, p in _migrations(version):
        conn.executescript(p.read_text(encoding="utf-8"))
        conn.execute(
            "INSERT INTO refinery_schema_history VALUES (?, ?, '2026-10-01T00:00:00Z', '0')", (v, name)
        )
    if sample:
        populate(conn, has_confidence=version >= 5)
    conn.commit()
    conn.close()
    return path


def populate(conn: sqlite3.Connection, has_confidence: bool = True) -> None:
    """採用率が手計算できるデータ。

    completions: 1,2 = model-a / doc-a、3,4 = model-b / doc-b
    採用(selected)ありは 1 と 3 だけ → リクエスト単位 2/4
    rank 0: 4 件中 1 採用(c1) / rank 1: 3 件中 1 採用(c3) / rank 2: 2 件中 0 採用
    """
    comps = [
        (1, "2026-10-01 10:00:00.100", "file:///a.txt", 3, 5, "model-a", "prompt one"),
        (2, "2026-10-01 11:00:00.100", "file:///a.txt", 4, 0, "model-a", "prompt two"),
        (3, "2026-10-02 09:00:00.100", "file:///b.txt", 1, 2, "model-b", "prompt three"),
        (4, "2026-10-03 09:00:00.100", "file:///b.txt", 2, 2, "model-b", "prompt four"),
    ]
    conn.executemany(
        "INSERT INTO completions (id, created_at, document_uri, cursor_line, cursor_character,"
        " model_name, prompt) VALUES (?,?,?,?,?,?,?)",
        comps,
    )
    # (completion_id, rank, candidate, selected, confidence)
    cands = [
        (1, 0, "採用された続き", 1, 0.9),
        (1, 1, "別案その一", 0, 0.5),
        (1, 2, "別案その二", 0, 0.2),
        (2, 0, "未採用A", 0, 0.8),
        (2, 1, "未採用B", 0, 0.4),
        (2, 2, "未採用C", 0, 0.1),
        (3, 0, "未採用D", 0, 0.7),
        (3, 1, "二番目が採用", 1, 0.6),
        (4, 0, "未採用E", 0, 0.3),
    ]
    for cid, rank, text, sel, conf in cands:
        if has_confidence:
            conn.execute(
                "INSERT INTO completion_candidates (completion_id, rank, candidate, selected, confidence)"
                " VALUES (?,?,?,?,?)",
                (cid, rank, text, sel, conf),
            )
        else:
            conn.execute(
                "INSERT INTO completion_candidates (completion_id, rank, candidate, selected)"
                " VALUES (?,?,?,?)",
                (cid, rank, text, sel),
            )

    conn.execute(
        "INSERT INTO code_actions (id, document_uri, mode, target_text, model_name, prompt, response)"
        " VALUES (1, 'file:///a.txt', 'rephrase', '対象の文', 'model-a', 'ca prompt', '{\"candidates\":[]}')"
    )
    for rank, text in enumerate(["書き換え案1", "書き換え案2"]):
        conn.execute(
            "INSERT INTO code_action_candidates (code_action_id, rank, candidate) VALUES (1, ?, ?)",
            (rank, text),
        )

    conn.execute(
        "INSERT INTO character_updates (id, document_uri, model_name, prompt, response)"
        " VALUES (1, 'file:///a.txt', 'model-a', 'cu prompt', 'cu response')"
    )
    conn.execute(
        "INSERT INTO character_update_sections (update_id, character_name, attribute, old_text,"
        " new_text, applied, skip_reason) VALUES (1, '太郎', '口調', NULL, 'ぶっきらぼう', 1, NULL)"
    )

    conn.execute(
        "INSERT INTO quality_reviews (id, document_uri, model_name, prompt, response)"
        " VALUES (1, 'file:///a.txt', 'model-a', 'qr prompt', 'qr response')"
    )
    conn.execute(
        "INSERT INTO quality_findings (document_uri, source, rule_code, severity, line, excerpt,"
        " message, review_id) VALUES ('file:///a.txt', 'rule', 'no-chain', 'information', 3,"
        " '抜粋', 'msg', NULL)"
    )
    conn.execute(
        "INSERT INTO quality_findings (document_uri, source, rule_code, severity, line, excerpt,"
        " message, review_id) VALUES ('file:///a.txt', 'llm', 'llm-review', 'warning', 5,"
        " '壮大な', 'llm msg', 1)"
    )

    # 各テーブルの行数(テストが期待値に使う)
    # completions 4 / completion_candidates 9 / code_actions 1 / code_action_candidates 2
    # character_updates 1 / character_update_sections 1 / quality_reviews 1 / quality_findings 2


def info(path: Path, mtime: int | None = 1_760_000_000_000):
    """取り込み時に渡すファイルの指紋。ブラウザが送る File.lastModified の代わりに固定値を使う。"""
    from shinasadame.etl import FileInfo

    return FileInfo(name=path.name, size=path.stat().st_size, mtime=mtime)


EXPECTED_COUNTS = {
    "completions": 4,
    "completion_candidates": 9,
    "code_actions": 1,
    "code_action_candidates": 2,
    "character_updates": 1,
    "character_update_sections": 1,
    "quality_reviews": 1,
    "quality_findings": 2,
}
