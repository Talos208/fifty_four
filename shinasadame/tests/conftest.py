import os
import tempfile

# tasks.huey は import 時に queue.db を DATA_DIR に作る。リポジトリ内に .data を作らないよう、
# shinasadame を import する前に一時ディレクトリへ向けておく。
os.environ["DATA_DIR"] = tempfile.mkdtemp(prefix="shinasadame-boot-")
os.environ.pop("OTEL_EXPORTER_OTLP_ENDPOINT", None)

import pytest  # noqa: E402

import fr_db  # noqa: E402
from shinasadame import db, tasks  # noqa: E402


@pytest.fixture(autouse=True)
def data_dir(tmp_path, monkeypatch):
    """テストごとに空の app.db を用意し、Huey は同期実行(immediate)にする。"""
    monkeypatch.setenv("DATA_DIR", str(tmp_path / "data"))
    db.dispose_engines()
    db.init_db()
    tasks.huey.immediate = True
    yield
    db.dispose_engines()


@pytest.fixture
def engine():
    return db.get_engine()


@pytest.fixture
def fr_v5(tmp_path):
    return fr_db.build(tmp_path / "v5.db", version=5)


@pytest.fixture
def fr_v4(tmp_path):
    return fr_db.build(tmp_path / "v4.db", version=4)
