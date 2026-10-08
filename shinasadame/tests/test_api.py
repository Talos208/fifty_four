import pytest
from fastapi.testclient import TestClient
from sqlalchemy import insert, select

from shinasadame import config, db, tasks
from shinasadame.app import app


@pytest.fixture
def client():
    # with を使って lifespan(init_db)を走らせる
    with TestClient(app) as c:
        yield c


MTIME = 1_760_000_000_000


def upload(client, path, name="fifty_four.db", mtime=MTIME):
    data = {} if mtime is None else {"file_mtime": str(mtime)}
    with open(path, "rb") as f:
        return client.post(
            "/api/imports", data=data, files={"file": (name, f, "application/octet-stream")}
        )


def pending_import(status="running"):
    now = db.now_iso()
    with db.get_engine().begin() as conn:
        return conn.execute(
            insert(db.imports).values(
                file_name="fifty_four.db", file_size=1, status=status, created_at=now, updated_at=now
            )
        ).inserted_primary_key[0]


def test_upload_import_redirects_to_acceptance(client, fr_v5):
    res = upload(client, fr_v5)
    assert res.status_code == 202
    import_id = res.json()["import_id"]

    # Huey は immediate なので、投入した時点で完了している
    status = client.get(f"/imports/{import_id}/status")
    assert status.status_code == 200
    sid = int(status.headers["HX-Redirect"].split("/")[2])
    assert status.headers["HX-Redirect"] == f"/s/{sid}/acceptance"

    # アップロードの一時ファイルは片付いている
    assert not config.upload_path(import_id).exists()
    assert not list(config.uploads_dir().glob("tmp-*"))


def test_broken_file_shows_reason_and_retry(client, tmp_path):
    bad = tmp_path / "bad.db"
    bad.write_bytes(b"not a database" * 50)
    import_id = upload(client, bad).json()["import_id"]

    html = client.get(f"/imports/{import_id}/status").text
    assert "取り込みに失敗しました" in html
    assert "retryImport()" in html  # 再試行ボタン
    assert not config.upload_path(import_id).exists()


def test_oversized_upload_is_413(client, fr_v5, monkeypatch):
    monkeypatch.setattr(config, "MAX_UPLOAD_BYTES", 1024)
    assert upload(client, fr_v5).status_code == 413
    assert not list(config.uploads_dir().iterdir())
    with db.get_engine().connect() as conn:
        assert conn.execute(select(db.imports)).first() is None


def test_lookup_finds_imported_file(client, fr_v5):
    size = fr_v5.stat().st_size
    q = {"name": "fifty_four.db", "size": size, "mtime": MTIME}
    assert client.get("/api/snapshots/lookup", params=q).json() == {"snapshot": None}

    upload(client, fr_v5)
    snap = client.get("/api/snapshots/lookup", params=q).json()["snapshot"]
    assert (snap["file_name"], snap["file_size"], snap["file_mtime"]) == ("fifty_four.db", size, MTIME)

    # 更新日時が変わった(LSP が書き足した)ファイルは未取り込み
    changed = {**q, "mtime": MTIME + 1}
    assert client.get("/api/snapshots/lookup", params=changed).json() == {"snapshot": None}


def test_upload_keeps_only_file_name(client, fr_v5):
    # ブラウザによっては filename にパスが付くことがあるが、名前だけ記録する
    upload(client, fr_v5, name=r"C:\Users\x\fifty_four.db", mtime=None)
    with db.get_engine().connect() as conn:
        snap = conn.execute(select(db.snapshots)).one()
    assert snap.file_name == "fifty_four.db"
    assert snap.file_mtime is None


def test_unknown_import_is_404(client):
    assert client.get("/imports/999/status").status_code == 404


def test_long_queued_import_hints_worker_is_down(client):
    # worker(huey_consumer)が動いていないと、ジョブは queued のまま進まない
    import_id = pending_import(status="queued")
    with db.get_engine().begin() as conn:
        conn.execute(
            db.imports.update()
            .where(db.imports.c.id == import_id)
            .values(created_at="2026-01-01T00:00:00+00:00")
        )
    html = client.get(f"/imports/{import_id}/status").text
    assert "huey_consumer" in html
    assert 'hx-trigger="every 1s"' in html  # worker を起動すればそのまま進むので、問い合わせは続ける


def test_fresh_queued_import_has_no_worker_hint(client):
    import_id = pending_import(status="queued")
    assert "huey_consumer" not in client.get(f"/imports/{import_id}/status").text


def test_pending_import_shows_polling_fragment(client):
    import_id = pending_import()
    html = client.get(f"/imports/{import_id}/status").text
    assert f'hx-get="/imports/{import_id}/status"' in html
    assert 'hx-trigger="every 1s"' in html


def test_stale_running_jobs_fail_on_worker_startup(client):
    import_id = pending_import()
    leftover = config.upload_path(import_id)
    leftover.write_bytes(b"x")

    tasks.fail_stale_imports()

    with db.get_engine().connect() as conn:
        row = conn.execute(select(db.imports).where(db.imports.c.id == import_id)).one()
    assert row.status == "failed"
    assert "再起動" in row.error
    assert not leftover.exists()


@pytest.fixture
def sid(client, fr_v5):
    upload(client, fr_v5)
    with db.get_engine().connect() as conn:
        return conn.execute(select(db.snapshots.c.id)).scalar_one()


def test_acceptance_api(client, sid):
    res = client.get(f"/api/s/{sid}/acceptance", params={"by": "rank"})
    assert res.status_code == 200
    rows = res.json()["rows"]
    assert [(r["key"], r["total"], r["accepted"]) for r in rows] == [(0, 4, 1), (1, 3, 1), (2, 2, 0)]

    filtered = client.get(f"/api/s/{sid}/acceptance", params={"by": "model", "model": "model-a"})
    assert [r["key"] for r in filtered.json()["rows"]] == ["model-a"]

    assert client.get(f"/api/s/{sid}/acceptance", params={"by": "bogus"}).status_code == 422
    assert client.get("/api/s/999/acceptance").status_code == 404


def test_pages_render(client, sid):
    page = client.get(f"/s/{sid}/acceptance", params={"from": "2026-10-01"}).text
    assert "下限値" in page
    assert "50.0%" in page
    assert '<span title="/a.txt">a.txt</span>' in page  # 文書別の表(URI ではなくファイル名)

    assert client.get("/").status_code == 200
    for table in ("completions", "code_actions", "character_updates", "quality_reviews", "quality_findings"):
        assert client.get(f"/s/{sid}/records/{table}").status_code == 200, table

    listing = client.get(f"/s/{sid}/records/completions", params={"q": "prompt one"}).text
    assert f"/s/{sid}/records/completions/1" in listing

    detail = client.get(f"/s/{sid}/records/completions/1").text
    assert "採用された続き" in detail
    assert "0.90" in detail

    assert client.get(f"/s/{sid}/records/completions/999").status_code == 404
    assert client.get(f"/s/{sid}/records/nope").status_code == 404
    assert client.get("/s/999/acceptance").status_code == 404


def test_acceptance_drills_down_by_document(client, sid):
    page = client.get(f"/s/{sid}/acceptance", params={"model": "model-b"}).text
    # 文書別の表の各行は、今の絞り込み条件を保ったまま、その文書に絞るリンクになっている
    assert 'href="?model=model-b&amp;doc=file%3A%2F%2F%2Fb.txt"' in page

    page = client.get(f"/s/{sid}/acceptance", params={"doc": "file:///a.txt"}).text
    assert "すべての文書に戻す" in page
    assert '<input type="hidden" name="doc" value="file:///a.txt">' in page
    assert '<span class="num">2</span>' in page  # a.txt の補完は 2 件
    assert "50.0%" in page
    # 文書を選んでいても、表には全文書が残る(別の文書へ切り替えられる)
    assert "doc=file%3A%2F%2F%2Fb.txt" in page
    assert 'class="current"' in page

    rows = client.get(
        f"/api/s/{sid}/acceptance", params={"by": "rank", "doc": "file:///b.txt"}
    ).json()["rows"]
    assert [(r["key"], r["total"], r["accepted"]) for r in rows] == [(0, 2, 0), (1, 1, 1)]


def test_document_uri_is_shown_as_japanese_file_name(client, sid):
    encoded = "file:///C:/Users/talos/writes/%E9%87%91%E5%89%9B/%E5%8E%9F%E7%A8%BF.txt"
    t = db.FR_TABLES["completions"]
    with db.get_engine().begin() as conn:
        conn.execute(t.update().where(t.c.id == 1).values(document_uri=encoded))

    for path in (
        f"/s/{sid}/acceptance",
        f"/s/{sid}/records/completions",
        f"/s/{sid}/records/completions/1",
    ):
        html = client.get(path).text
        assert "原稿.txt" in html, path
        assert "金剛" in html, path  # 親フォルダ名
        assert "%E5%8E%9F" not in html, path

    # 詳細ではフルパスも出す
    assert "C:/Users/talos/writes/金剛/原稿.txt" in client.get(f"/s/{sid}/records/completions/1").text


def test_newer_schema_warning_is_shown(client, sid):
    with db.get_engine().begin() as conn:
        conn.execute(db.snapshots.update().values(schema_version=6))
    assert "V6" in client.get(f"/s/{sid}/acceptance").text


def test_static_assets_are_bundled(client):
    for name in ("htmx.min.js", "echarts.min.js", "app.js", "style.css"):
        assert client.get(f"/static/{name}").status_code == 200, name
    assert client.get("/healthz").text == "ok"
