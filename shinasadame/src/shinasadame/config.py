"""設定。環境変数を呼び出しのたびに読むので、テストで `DATA_DIR` を差し替えられる。"""

import os
from pathlib import Path

# アップロード上限(FlightRecorder の DB 1 ファイル分)
MAX_UPLOAD_BYTES = 256 * 1024 * 1024

# 残すスナップショット数(全体で新しい順)
KEEP_SNAPSHOTS = 10

# この版までのスキーマ(lsp/migrations/V*.sql)の列を取り込める
KNOWN_SCHEMA_VERSION = 5


def data_dir() -> Path:
    path = Path(os.environ.get("DATA_DIR", ".data"))
    path.mkdir(parents=True, exist_ok=True)
    return path


def app_db_path() -> Path:
    return data_dir() / "app.db"


def queue_db_path() -> Path:
    return data_dir() / "queue.db"


def uploads_dir() -> Path:
    path = data_dir() / "uploads"
    path.mkdir(parents=True, exist_ok=True)
    return path


def upload_path(import_id: int) -> Path:
    return uploads_dir() / f"{import_id}.db"


def otlp_endpoint() -> str | None:
    return os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT") or None
