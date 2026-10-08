"""FlightRecorder の DB を検証し、app.db の `fr_*` テーブルへスナップショットとしてコピーする。

アップロードされたファイルは読み取り専用(`immutable=1`)で開く。WAL モードの DB を
ブラウザが本体ファイルだけ読んで送ってきた場合も、-wal/-shm を作ろうとせず開ける。
"""

import sqlite3
from dataclasses import dataclass
from pathlib import Path

from sqlalchemy import Connection, Engine, delete, func, insert, select, update

from . import config, db

BATCH = 5000


class ImportFailed(Exception):
    """取り込めなかった理由をそのままユーザーに見せるための例外。"""


def _open_source(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{path.as_posix()}?immutable=1", uri=True)


def validate(src: sqlite3.Connection) -> int:
    """FlightRecorder の DB であることを確かめ、スキーマの最大バージョンを返す。"""
    try:
        result = src.execute("PRAGMA integrity_check").fetchall()
        if result != [("ok",)]:
            raise ImportFailed(f"integrity_check が失敗しました: {result[:3]}")
        has_history = src.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='refinery_schema_history'"
        ).fetchone()
        if not has_history:
            raise ImportFailed("FlightRecorder の DB ではありません(refinery_schema_history がありません)")
        row = src.execute("SELECT max(version) FROM refinery_schema_history").fetchone()
    except sqlite3.DatabaseError as e:
        raise ImportFailed(f"SQLite の DB として読めません: {e}") from e
    if row is None or row[0] is None:
        raise ImportFailed("スキーマのバージョンを取得できません(refinery_schema_history が空です)")
    return int(row[0])


def _copy_table(src: sqlite3.Connection, conn: Connection, name: str, snapshot_id: int) -> int:
    """1 テーブル分をコピーして件数を返す。元に無い列は NULL のまま、元にしか無い列は捨てる。"""
    src_cols = [r[1] for r in src.execute(f'PRAGMA table_info("{name}")')]
    if not src_cols:
        return 0
    wanted = ["id", *[c for c, _ in db.FR_COLUMNS[name]]]
    use = [c for c in wanted if c in src_cols]
    if "id" not in use:
        return 0
    table = db.FR_TABLES[name]
    cols_sql = ", ".join(f'"{c}"' for c in use)
    cur = src.execute(f'SELECT {cols_sql} FROM "{name}"')
    total = 0
    while rows := cur.fetchmany(BATCH):
        conn.execute(
            insert(table),
            [{"snapshot_id": snapshot_id, **dict(zip(use, row, strict=True))} for row in rows],
        )
        total += len(rows)
    return total


def _prune(conn: Connection) -> None:
    """全体で最新 KEEP_SNAPSHOTS 件だけ残し、古いものは行ごと消す。"""
    ids = conn.execute(
        select(db.snapshots.c.id).order_by(db.snapshots.c.id.desc())
    ).scalars().all()
    stale = ids[config.KEEP_SNAPSHOTS :]
    if not stale:
        return
    for table in db.FR_TABLES.values():
        conn.execute(delete(table).where(table.c.snapshot_id.in_(stale)))
    conn.execute(delete(db.snapshots).where(db.snapshots.c.id.in_(stale)))
    conn.execute(
        update(db.imports).where(db.imports.c.snapshot_id.in_(stale)).values(snapshot_id=None)
    )


@dataclass(frozen=True)
class FileInfo:
    """取り込んだファイルの指紋。同じ指紋のスナップショットが残っていれば取り込み済みとみなす。

    中身のハッシュではなくメタデータを使うのは、ブラウザがファイルを読まずに判定できるから
    (256MB を読んでハッシュを取ると、スキップしたい読み込みそのものが発生してしまう)。
    """

    name: str
    size: int
    mtime: int | None  # File.lastModified(エポックミリ秒)


def find_snapshot(engine: Engine, file: FileInfo) -> dict | None:
    """同じファイル(名前・サイズ・最終更新日時が一致)の取り込み済みスナップショットのうち最新のもの。

    最終更新日時が分からないファイルは、同一と言い切れないので常に未取り込み扱い。
    """
    if file.mtime is None:
        return None
    s = db.snapshots
    with engine.connect() as conn:
        row = conn.execute(
            select(s)
            .where(s.c.file_name == file.name, s.c.file_size == file.size, s.c.file_mtime == file.mtime)
            .order_by(s.c.id.desc())
            .limit(1)
        ).first()
    return dict(row._mapping) if row else None


def import_snapshot(engine: Engine, path: Path, file: FileInfo) -> int:
    """`path` の FlightRecorder DB を取り込み、新しい snapshot_id を返す。"""
    try:
        src = _open_source(path)
    except sqlite3.Error as e:
        raise ImportFailed(f"ファイルを開けません: {e}") from e
    try:
        version = validate(src)
        # スナップショットの作成・コピー・古い分の削除は 1 トランザクション。途中で落ちても行は残らない。
        with engine.begin() as conn:
            snapshot_id = conn.execute(
                insert(db.snapshots).values(
                    file_name=file.name,
                    file_size=file.size,
                    file_mtime=file.mtime,
                    schema_version=version,
                    imported_at=db.now_iso(),
                )
            ).inserted_primary_key[0]
            for name in db.FR_TABLES:
                try:
                    _copy_table(src, conn, name, snapshot_id)
                except sqlite3.DatabaseError as e:
                    raise ImportFailed(f"{name} を読めません: {e}") from e
            _prune(conn)
        return snapshot_id
    finally:
        src.close()


def count_rows(engine: Engine, snapshot_id: int, name: str) -> int:
    table = db.FR_TABLES[name]
    with engine.connect() as conn:
        return conn.execute(
            select(func.count()).select_from(table).where(table.c.snapshot_id == snapshot_id)
        ).scalar_one()
