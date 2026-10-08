"""FastAPI 本体。ルーティングと画面の組み立てだけを持ち、処理は各モジュールに任せる。"""

import uuid
from urllib.parse import urlencode
from contextlib import asynccontextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path, PureWindowsPath

from fastapi import FastAPI, File, Form, HTTPException, Query, Request, UploadFile
from fastapi.responses import JSONResponse, PlainTextResponse, Response
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from sqlalchemy import insert, select

from . import config, db, etl, records, tasks
from .analysis import acceptance as acc
from .display import doc_name
from .telemetry import setup_telemetry

BASE = Path(__file__).parent

# 取り込みジョブがこれ以上 queued のままなら、worker が動いていないと知らせる
WORKER_STALL_AFTER = timedelta(seconds=10)
templates = Jinja2Templates(directory=BASE / "templates")


def _pct(value: float | None) -> str:
    return "—" if value is None else f"{value * 100:.1f}%"


def _mtime(ms: int | None) -> str:
    """File.lastModified(エポックミリ秒)を、imported_at と同じ UTC の表記にする。"""
    if ms is None:
        return "不明"
    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%d %H:%M")


templates.env.filters["pct"] = _pct
templates.env.filters["mtime"] = _mtime
templates.env.filters["doc_name"] = doc_name


@asynccontextmanager
async def lifespan(_app: FastAPI):
    db.init_db()
    yield


# トレース送信は setup_telemetry が gRPC で行う。FastAPI 本体の自動設定(OTEL_* を読んで
# http/protobuf の exporter を足す)は止める。gRPC 設定の環境では失敗の警告を出し、
# http/protobuf の環境では setup_telemetry と二重に送ってしまうため。
app = FastAPI(title="shinasadame", lifespan=lifespan, telemetry={"auto_configure": False})
app.mount("/static", StaticFiles(directory=BASE / "static"), name="static")
setup_telemetry(app)


def _snapshots(limit: int = 30) -> list[dict]:
    with db.get_engine().connect() as conn:
        rows = conn.execute(
            select(db.snapshots).order_by(db.snapshots.c.id.desc()).limit(limit)
        )
        return [dict(r._mapping) for r in rows]


def _render(request: Request, name: str, sid: int | None = None, **ctx):
    ctx.update(
        sid=sid,
        snapshots=_snapshots(),
        known_version=config.KNOWN_SCHEMA_VERSION,
        record_specs=records.SPECS,
    )
    return templates.TemplateResponse(request, name, ctx)


def _snapshot_or_404(sid: int) -> dict:
    with db.get_engine().connect() as conn:
        row = conn.execute(select(db.snapshots).where(db.snapshots.c.id == sid)).first()
    if row is None:
        raise HTTPException(404, "スナップショットが見つかりません")
    return dict(row._mapping)


@app.get("/healthz", response_class=PlainTextResponse)
def healthz() -> str:
    return "ok"


@app.get("/")
def index(request: Request):
    return _render(request, "index.html")


@app.get("/api/snapshots/lookup")
def lookup_snapshot(name: str, size: int, mtime: int | None = None):
    """同じファイルの取り込み済みスナップショットが残っていれば返す。ブラウザがアップロード前に
    問い合わせ、取り込み済みならスキップ(既存スナップショットを開く)を選べるようにする。"""
    found = etl.find_snapshot(db.get_engine(), etl.FileInfo(name=name, size=size, mtime=mtime))
    return {"snapshot": found}


@app.post("/api/imports", status_code=202)
def create_import(file: UploadFile = File(...), file_mtime: int | None = Form(None)):
    # Windows 形式のパスが付いてきても名前だけにする(Linux の Path は "\" を区切りとみなさない)
    file_name = PureWindowsPath(file.filename or "").name or "upload.db"
    tmp = config.uploads_dir() / f"tmp-{uuid.uuid4().hex}"
    size = 0
    try:
        with tmp.open("wb") as out:
            while chunk := file.file.read(1024 * 1024):
                size += len(chunk)
                if size > config.MAX_UPLOAD_BYTES:
                    raise HTTPException(413, "ファイルが大きすぎます(上限 256MB)")
                out.write(chunk)
        now = db.now_iso()
        with db.get_engine().begin() as conn:
            import_id = conn.execute(
                insert(db.imports).values(
                    file_name=file_name,
                    file_size=size,
                    file_mtime=file_mtime,
                    status="queued",
                    created_at=now,
                    updated_at=now,
                )
            ).inserted_primary_key[0]
        tmp.replace(config.upload_path(import_id))
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
    tasks.run_import(import_id)
    return JSONResponse({"import_id": import_id}, status_code=202)


@app.get("/imports/{import_id}/status")
def import_status(request: Request, import_id: int):
    with db.get_engine().connect() as conn:
        row = conn.execute(select(db.imports).where(db.imports.c.id == import_id)).first()
    if row is None:
        raise HTTPException(404, "取り込みジョブが見つかりません")
    imp = dict(row._mapping)
    if imp["status"] == "done" and imp["snapshot_id"] is not None:
        # 空の 200 + HX-Redirect で、htmx にページ遷移させる
        return Response(headers={"HX-Redirect": f"/s/{imp['snapshot_id']}/acceptance"})
    # worker が動いていれば queued はすぐ running になる。待機が続くなら worker が止まっている
    waited = datetime.now(UTC) - datetime.fromisoformat(imp["created_at"])
    stalled = imp["status"] == "queued" and waited > WORKER_STALL_AFTER
    return _render(request, "_import_status.html", imp=imp, stalled=stalled)


def _acceptance_args(
    date_from: str | None, date_to: str | None, model: str | None, document: str | None = None
) -> dict:
    return {
        "date_from": date_from or None,
        "date_to": date_to or None,
        "model": model or None,
        "document": document or None,
    }


@app.get("/s/{sid}/acceptance")
def acceptance_page(
    request: Request,
    sid: int,
    date_from: str | None = Query(None, alias="from"),
    date_to: str | None = Query(None, alias="to"),
    model: str | None = None,
    doc: str | None = None,
):
    snapshot = _snapshot_or_404(sid)
    engine = db.get_engine()
    args = _acceptance_args(date_from, date_to, model, doc)
    f = {"from": date_from or "", "to": date_to or "", "model": model or "", "doc": doc or ""}

    def qs(**overrides: str) -> str:
        """今の絞り込み条件の一部だけ差し替えたクエリ文字列(空の条件は落とす)。"""
        merged = {k: v for k, v in {**f, **overrides}.items() if v}
        return "?" + urlencode(merged) if merged else "?"

    return _render(
        request,
        "acceptance.html",
        active="acceptance",
        sid=sid,
        snapshot=snapshot,
        summary=acc.summary(engine, sid, **args),
        # 文書別の表は、文書を選んでいても全文書を出す(別の文書へ切り替えられるように)
        documents=acc.acceptance(engine, sid, "document", **{**args, "document": None}),
        models=acc.models(engine, sid),
        f=f,
        qs=qs,
    )


@app.get("/api/s/{sid}/acceptance")
def acceptance_api(
    sid: int,
    by: str = "day",
    date_from: str | None = Query(None, alias="from"),
    date_to: str | None = Query(None, alias="to"),
    model: str | None = None,
    doc: str | None = None,
):
    _snapshot_or_404(sid)
    if by not in acc.BY:
        raise HTTPException(422, f"by は {list(acc.BY)} のどれかです")
    rows = acc.acceptance(
        db.get_engine(), sid, by, **_acceptance_args(date_from, date_to, model, doc)
    )
    return {"by": by, "rows": rows}


def _spec_or_404(table: str) -> records.Spec:
    spec = records.SPECS.get(table)
    if spec is None:
        raise HTTPException(404, "そのテーブルは閲覧できません")
    return spec


@app.get("/s/{sid}/records/{table}")
def records_page(request: Request, sid: int, table: str, q: str = "", page: int = 1):
    snapshot = _snapshot_or_404(sid)
    spec = _spec_or_404(table)
    rows, total = records.list_records(db.get_engine(), sid, table, q.strip(), page)
    pages = max((total + records.PAGE_SIZE - 1) // records.PAGE_SIZE, 1)
    return _render(
        request,
        "records_list.html",
        sid=sid,
        snapshot=snapshot,
        table=table,
        spec=spec,
        rows=rows,
        total=total,
        q=q,
        page=max(page, 1),
        pages=pages,
    )


@app.get("/s/{sid}/records/{table}/{record_id}")
def record_detail(request: Request, sid: int, table: str, record_id: int):
    snapshot = _snapshot_or_404(sid)
    spec = _spec_or_404(table)
    detail = records.get_record(db.get_engine(), sid, table, record_id)
    if detail is None:
        raise HTTPException(404, "レコードが見つかりません")
    return _render(
        request,
        "record_detail.html",
        sid=sid,
        snapshot=snapshot,
        table=table,
        spec=spec,
        record_id=record_id,
        **detail,
    )
