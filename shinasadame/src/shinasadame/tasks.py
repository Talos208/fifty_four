"""Huey のキューと取り込みジョブ。web と worker で同じモジュールを共有する。"""

import logging

from huey import SqliteHuey
from sqlalchemy import select, update

from . import config, db, etl

log = logging.getLogger(__name__)

huey = SqliteHuey("shinasadame", filename=str(config.queue_db_path()))


def set_status(import_id: int, status: str, *, error: str | None = None, snapshot_id: int | None = None):
    with db.get_engine().begin() as conn:
        conn.execute(
            update(db.imports)
            .where(db.imports.c.id == import_id)
            .values(status=status, error=error, snapshot_id=snapshot_id, updated_at=db.now_iso())
        )


@huey.task()
def run_import(import_id: int) -> None:
    engine = db.get_engine()
    path = config.upload_path(import_id)
    i = db.imports
    with engine.connect() as conn:
        row = conn.execute(
            select(i.c.file_name, i.c.file_size, i.c.file_mtime).where(i.c.id == import_id)
        ).first()
    if row is None:
        log.warning("import %s not found", import_id)
        path.unlink(missing_ok=True)
        return
    set_status(import_id, "running")
    try:
        file = etl.FileInfo(name=row.file_name, size=row.file_size, mtime=row.file_mtime)
        snapshot_id = etl.import_snapshot(engine, path, file)
        set_status(import_id, "done", snapshot_id=snapshot_id)
    except etl.ImportFailed as e:
        set_status(import_id, "failed", error=str(e))
    except Exception as e:  # noqa: BLE001 - 想定外の失敗も画面に理由を出す
        log.exception("import %s failed", import_id)
        set_status(import_id, "failed", error=f"{type(e).__name__}: {e}")
    finally:
        path.unlink(missing_ok=True)


@huey.on_startup()
def fail_stale_imports() -> None:
    """worker が落ちて `running` のまま残ったジョブを失敗にする(行はトランザクションで残らない)。"""
    db.init_db()
    with db.get_engine().begin() as conn:
        ids = conn.execute(
            select(db.imports.c.id).where(db.imports.c.status == "running")
        ).scalars().all()
        for import_id in ids:
            conn.execute(
                update(db.imports)
                .where(db.imports.c.id == import_id)
                .values(
                    status="failed",
                    error="worker が再起動したため中断されました。もう一度取り込んでください",
                    updated_at=db.now_iso(),
                )
            )
            config.upload_path(import_id).unlink(missing_ok=True)
