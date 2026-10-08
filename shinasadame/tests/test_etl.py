import sqlite3

import pytest
from sqlalchemy import func, select

import fr_db
from shinasadame import config, db, etl


def count(engine, table, **where):
    t = db.FR_TABLES[table]
    stmt = select(func.count()).select_from(t)
    for col, val in where.items():
        stmt = stmt.where(t.c[col] == val)
    with engine.connect() as conn:
        return conn.execute(stmt).scalar_one()


def snapshot_count(engine):
    with engine.connect() as conn:
        return conn.execute(select(func.count()).select_from(db.snapshots)).scalar_one()


def test_import_copies_every_table(engine, fr_v5):
    sid = etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))
    for table, expected in fr_db.EXPECTED_COUNTS.items():
        assert count(engine, table, snapshot_id=sid) == expected, table
    with engine.connect() as conn:
        snap = conn.execute(select(db.snapshots).where(db.snapshots.c.id == sid)).one()
    assert snap.schema_version == 5
    assert (snap.file_name, snap.file_size, snap.file_mtime) == (
        "v5.db",
        fr_v5.stat().st_size,
        fr_db.info(fr_v5).mtime,
    )


def test_confidence_is_preserved(engine, fr_v5):
    sid = etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))
    t = db.FR_TABLES["completion_candidates"]
    with engine.connect() as conn:
        got = conn.execute(
            select(t.c.confidence).where(
                t.c.snapshot_id == sid, t.c.completion_id == 1, t.c.rank == 0
            )
        ).scalar_one()
    assert got == pytest.approx(0.9)


def test_old_schema_fills_missing_columns_with_null(engine, fr_v4):
    sid = etl.import_snapshot(engine, fr_v4, fr_db.info(fr_v4))
    t = db.FR_TABLES["completion_candidates"]
    with engine.connect() as conn:
        confs = conn.execute(select(t.c.confidence).where(t.c.snapshot_id == sid)).scalars().all()
        version = conn.execute(
            select(db.snapshots.c.schema_version).where(db.snapshots.c.id == sid)
        ).scalar_one()
    assert len(confs) == 9
    assert set(confs) == {None}
    assert version == 4


def test_v1_only_database_imports_what_exists(engine, tmp_path):
    path = fr_db.build(tmp_path / "v1.db", version=1, sample=False)
    sid = etl.import_snapshot(engine, path, fr_db.info(path))
    assert count(engine, "completions", snapshot_id=sid) == 0
    assert snapshot_count(engine) == 1


def test_newer_schema_imports_known_columns_only(engine, tmp_path):
    path = fr_db.build(tmp_path / "v6.db", version=5)
    conn = sqlite3.connect(path)
    conn.execute("ALTER TABLE completions ADD COLUMN template_kind TEXT")
    conn.execute("INSERT INTO refinery_schema_history VALUES (6, 'template', 'x', '0')")
    conn.commit()
    conn.close()

    sid = etl.import_snapshot(engine, path, fr_db.info(path))
    with engine.connect() as c:
        version = c.execute(
            select(db.snapshots.c.schema_version).where(db.snapshots.c.id == sid)
        ).scalar_one()
    assert version == 6 > config.KNOWN_SCHEMA_VERSION
    assert count(engine, "completions", snapshot_id=sid) == 4


def test_not_a_flight_recorder_db_fails_and_leaves_nothing(engine, tmp_path):
    path = tmp_path / "other.db"
    conn = sqlite3.connect(path)
    conn.execute("CREATE TABLE foo (x)")
    conn.commit()
    conn.close()

    with pytest.raises(etl.ImportFailed, match="FlightRecorder"):
        etl.import_snapshot(engine, path, fr_db.info(path))
    assert snapshot_count(engine) == 0


def test_garbage_file_fails(engine, tmp_path):
    path = tmp_path / "garbage.db"
    path.write_bytes(b"this is not sqlite" * 100)
    with pytest.raises(etl.ImportFailed):
        etl.import_snapshot(engine, path, fr_db.info(path))
    assert snapshot_count(engine) == 0


def test_failure_midway_rolls_back_everything(engine, fr_v5, monkeypatch):
    real = etl._copy_table

    def flaky(src, conn, name, snapshot_id):
        if name == "code_actions":
            raise sqlite3.DatabaseError("boom")
        return real(src, conn, name, snapshot_id)

    monkeypatch.setattr(etl, "_copy_table", flaky)
    with pytest.raises(etl.ImportFailed, match="code_actions"):
        etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))

    assert snapshot_count(engine) == 0
    for table in db.FR_TABLES:
        assert count(engine, table) == 0, table


def test_prune_keeps_latest_snapshots_overall(engine, fr_v5, fr_v4):
    # ファイルが違っても区別せず、全体で新しい順に KEEP_SNAPSHOTS 件だけ残る
    ids = [etl.import_snapshot(engine, fr_v4, fr_db.info(fr_v4))]
    ids += [
        etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5)) for _ in range(config.KEEP_SNAPSHOTS + 1)
    ]

    assert snapshot_count(engine) == config.KEEP_SNAPSHOTS
    # 古い 2 件(最初の v4 を含む)は行ごと消え、新しい方は残る
    for old in ids[:2]:
        assert count(engine, "completions", snapshot_id=old) == 0
        assert count(engine, "completion_candidates", snapshot_id=old) == 0
    for kept in ids[2:]:
        assert count(engine, "completions", snapshot_id=kept) == 4


def test_find_snapshot_matches_same_file_only(engine, fr_v5):
    file = fr_db.info(fr_v5)
    assert etl.find_snapshot(engine, file) is None

    first = etl.import_snapshot(engine, fr_v5, file)
    second = etl.import_snapshot(engine, fr_v5, file)
    # 同じファイルを取り込み直していれば、新しい方を返す
    assert etl.find_snapshot(engine, file)["id"] == second != first

    # 名前・サイズ・更新日時のどれかが違えば別のファイル
    assert etl.find_snapshot(engine, etl.FileInfo(file.name, file.size, file.mtime + 1)) is None
    assert etl.find_snapshot(engine, etl.FileInfo(file.name, file.size + 1, file.mtime)) is None
    assert etl.find_snapshot(engine, etl.FileInfo("other.db", file.size, file.mtime)) is None
    # 更新日時が分からないファイルは、同一と言い切れないので取り込み済み扱いにしない
    assert etl.find_snapshot(engine, etl.FileInfo(file.name, file.size, None)) is None


def test_find_snapshot_ignores_pruned(engine, fr_v5, fr_v4):
    old = fr_db.info(fr_v4)
    etl.import_snapshot(engine, fr_v4, old)
    for _ in range(config.KEEP_SNAPSHOTS):
        etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))
    # 消えたスナップショットは「取り込み済みのデータが残っている」とは言えない
    assert etl.find_snapshot(engine, old) is None
