"""app.db(SQLAlchemy Core)のスキーマとエンジン。

app.db の中身はすべて FlightRecorder の DB から再生成できる派生データなので、
マイグレーション機構は持たない。起動時にスキーマ定義のハッシュを `meta` と比べ、
変わっていたらテーブルを作り直す。
"""

import hashlib
import sqlite3
import threading
from datetime import UTC, datetime
from pathlib import Path

from sqlalchemy import (
    Column,
    Engine,
    Float,
    Index,
    Integer,
    MetaData,
    Table,
    Text,
    create_engine,
    event,
)
from sqlalchemy.dialects import sqlite as sqlite_dialect
from sqlalchemy.schema import CreateIndex, CreateTable

from . import config

metadata = MetaData()

# FlightRecorder の各テーブルの列(id 以外)。V1〜V5 の和集合。
# 旧スキーマに無い列(`confidence` など)は取り込み時に NULL になるので、すべて NULL 可。
FR_COLUMNS: dict[str, list[tuple[str, type]]] = {
    "completions": [
        ("created_at", Text),
        ("document_uri", Text),
        ("cursor_line", Integer),
        ("cursor_character", Integer),
        ("model_name", Text),
        ("prompt", Text),
    ],
    "completion_candidates": [
        ("completion_id", Integer),
        ("rank", Integer),
        ("candidate", Text),
        ("selected", Integer),
        ("confidence", Float),
    ],
    "code_actions": [
        ("created_at", Text),
        ("document_uri", Text),
        ("mode", Text),
        ("target_text", Text),
        ("model_name", Text),
        ("prompt", Text),
        ("response", Text),
    ],
    "code_action_candidates": [
        ("code_action_id", Integer),
        ("rank", Integer),
        ("candidate", Text),
        ("selected", Integer),
        ("confidence", Float),
    ],
    "character_updates": [
        ("started_at", Text),
        ("completed_at", Text),
        ("document_uri", Text),
        ("model_name", Text),
        ("prompt", Text),
        ("response", Text),
    ],
    "character_update_sections": [
        ("update_id", Integer),
        ("character_name", Text),
        ("attribute", Text),
        ("old_text", Text),
        ("new_text", Text),
        ("applied", Integer),
        ("skip_reason", Text),
    ],
    "quality_reviews": [
        ("created_at", Text),
        ("document_uri", Text),
        ("model_name", Text),
        ("prompt", Text),
        ("response", Text),
    ],
    "quality_findings": [
        ("created_at", Text),
        ("document_uri", Text),
        ("source", Text),
        ("rule_code", Text),
        ("severity", Text),
        ("line", Integer),
        ("excerpt", Text),
        ("message", Text),
        ("review_id", Integer),
    ],
}

# 子テーブルの親参照列。(snapshot_id, 親 id) で引くので索引を張る。
_CHILD_FK = {
    "completion_candidates": "completion_id",
    "code_action_candidates": "code_action_id",
    "character_update_sections": "update_id",
}

imports = Table(
    "imports",
    metadata,
    Column("id", Integer, primary_key=True, autoincrement=True),
    # 取り込んだファイルの指紋(名前・サイズ・最終更新日時)。snapshots にも引き継ぐ
    Column("file_name", Text, nullable=False),
    Column("file_size", Integer, nullable=False),
    Column("file_mtime", Integer),  # ブラウザの File.lastModified(エポックミリ秒)。不明なら NULL
    Column("status", Text, nullable=False),  # queued / running / done / failed
    Column("error", Text),
    Column("snapshot_id", Integer),
    Column("created_at", Text, nullable=False),
    Column("updated_at", Text, nullable=False),
)

snapshots = Table(
    "snapshots",
    metadata,
    Column("id", Integer, primary_key=True, autoincrement=True),
    Column("file_name", Text, nullable=False),
    Column("file_size", Integer, nullable=False),
    Column("file_mtime", Integer),
    Column("schema_version", Integer, nullable=False),
    Column("imported_at", Text, nullable=False),
)

meta = Table(
    "meta",
    metadata,
    Column("key", Text, primary_key=True),
    Column("value", Text, nullable=False),
)


def _fr_table(name: str, columns: list[tuple[str, type]]) -> Table:
    table = Table(
        f"fr_{name}",
        metadata,
        Column("snapshot_id", Integer, primary_key=True, autoincrement=False),
        Column("id", Integer, primary_key=True, autoincrement=False),
        *[Column(col, typ) for col, typ in columns],
    )
    fk = _CHILD_FK.get(name)
    if fk:
        Index(f"ix_fr_{name}_parent", table.c.snapshot_id, table.c[fk])
    return table


# 元のテーブル名 -> fr_ テーブル
FR_TABLES: dict[str, Table] = {name: _fr_table(name, cols) for name, cols in FR_COLUMNS.items()}

# 取り込み済みかどうかの判定(find_snapshot)に使う
Index("ix_snapshots_file", snapshots.c.file_name, snapshots.c.file_size, snapshots.c.file_mtime)


def now_iso() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


def _ddl_statements() -> list[str]:
    dialect = sqlite_dialect.dialect()
    stmts = [str(CreateTable(t).compile(dialect=dialect)).strip() for t in metadata.sorted_tables]
    for t in metadata.sorted_tables:
        for ix in sorted(t.indexes, key=lambda i: i.name or ""):
            stmts.append(str(CreateIndex(ix).compile(dialect=dialect)).strip())
    return stmts


def schema_hash() -> str:
    return hashlib.sha256("\n".join(_ddl_statements()).encode()).hexdigest()


def init_db(path: Path | None = None) -> None:
    """スキーマが無い/古いときだけ作り直す。web と worker が同時に呼んでも
    `BEGIN IMMEDIATE` で直列化されるので、片方が作り終えてから残りが確認する。"""
    path = path or config.app_db_path()
    conn = sqlite3.connect(path, timeout=30, isolation_level=None)
    try:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("BEGIN IMMEDIATE")
        try:
            current = None
            has_meta = conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'"
            ).fetchone()
            if has_meta:
                row = conn.execute("SELECT value FROM meta WHERE key='schema_hash'").fetchone()
                current = row[0] if row else None
            wanted = schema_hash()
            if current != wanted:
                tables = [
                    r[0]
                    for r in conn.execute(
                        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
                    )
                ]
                for name in tables:
                    conn.execute(f'DROP TABLE IF EXISTS "{name}"')
                for stmt in _ddl_statements():
                    conn.execute(stmt)
                conn.execute("INSERT INTO meta (key, value) VALUES ('schema_hash', ?)", (wanted,))
            conn.execute("COMMIT")
        except BaseException:
            conn.execute("ROLLBACK")
            raise
    finally:
        conn.close()


_engines: dict[Path, Engine] = {}
_lock = threading.Lock()


def get_engine(path: Path | None = None) -> Engine:
    path = (path or config.app_db_path()).resolve()
    with _lock:
        engine = _engines.get(path)
        if engine is None:
            engine = create_engine(f"sqlite:///{path}")

            @event.listens_for(engine, "connect")
            def _pragmas(dbapi_conn, _record):  # noqa: ANN001
                cur = dbapi_conn.cursor()
                cur.execute("PRAGMA busy_timeout=30000")
                cur.execute("PRAGMA synchronous=NORMAL")
                cur.close()

            _engines[path] = engine
        return engine


def dispose_engines() -> None:
    with _lock:
        for engine in _engines.values():
            engine.dispose()
        _engines.clear()
