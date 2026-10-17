"""設定。環境変数を呼び出しのたびに読むので、テストで `DATA_DIR` を差し替えられる。"""

import os
from pathlib import Path

from . import model_catalog

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


def replay_url() -> str:
    return os.environ.get("REPLAY_URL", "http://replay:8080").rstrip("/")


def replay_default_timeout_secs() -> int:
    try:
        value = int(os.environ.get("REPLAY_DEFAULT_TIMEOUT_SECS", "120"))
    except ValueError:
        return 120
    return value if value > 0 else 120


def lmstudio_host() -> str:
    """LM Studio 系モデルを replay に渡すときの `host:port`。replay Pod から見えるアドレス。"""
    return os.environ.get("REPLAY_LMSTUDIO_HOST", "").strip() or "host.docker.internal:1234"


def model_choices() -> list[str]:
    """画面の選択肢にする `provider/model`。

    環境変数 `SHINASADAME_MODELS`(カンマ区切り)があればそれを優先する(明示的な上書き)。
    無ければ `models.json`(tools/update_models.py が作る)。
    """
    raw = os.environ.get("SHINASADAME_MODELS", "")
    chosen = [m.strip() for m in raw.split(",") if m.strip()]
    return chosen or model_catalog.load_catalog()
