import logging

from fastapi.testclient import TestClient

from shinasadame.app import app


def test_fastapi_auto_telemetry_does_not_run(monkeypatch, caplog):
    """トレース送信は telemetry.setup_telemetry が gRPC で行う。FastAPI 本体の自動設定
    (OTEL_* 環境変数を読んで http/protobuf の exporter を足す)が動くと、gRPC 設定の環境では
    失敗の警告を出し、http/protobuf の環境では二重送信になる。"""
    # LSP 用にユーザー環境変数へ設定されている値(Windows で実際に入っていた組み合わせ)
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317")
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")
    monkeypatch.setenv("OTEL_METRICS_EXPORTER", "otlp")
    monkeypatch.setenv("OTEL_LOGS_EXPORTER", "otlp")

    with caplog.at_level(logging.WARNING), TestClient(app) as client:
        assert client.get("/healthz").text == "ok"

    assert "automatic telemetry" not in caplog.text


# ---- 送信側の計装(replay への httpx 呼び出しと SQLAlchemy) ----

import pytest  # noqa: E402
import sqlalchemy  # noqa: E402
from opentelemetry.instrumentation.httpx import HTTPXClientInstrumentor  # noqa: E402
from opentelemetry.instrumentation.sqlalchemy import SQLAlchemyInstrumentor  # noqa: E402
from opentelemetry.sdk.trace import TracerProvider  # noqa: E402
from opentelemetry.sdk.trace.export import SimpleSpanProcessor  # noqa: E402
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter  # noqa: E402
from sqlalchemy import text  # noqa: E402

from shinasadame import telemetry  # noqa: E402
from shinasadame.replay import ReplayClient, ReplayUnavailable  # noqa: E402


@pytest.fixture
def spans():
    """メモリ上の exporter に流す TracerProvider で計装を有効にし、終わったら必ず元に戻す
    (どちらの計装もプロセス全体に効くので、他のテストへ漏らさない)。"""
    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    telemetry.instrument_clients(provider)
    exporter.provider = provider  # テストが自分でスパンを作れるように渡す
    yield exporter
    HTTPXClientInstrumentor().uninstrument()
    SQLAlchemyInstrumentor().uninstrument()


def scopes(exporter) -> set[str]:
    return {s.instrumentation_scope.name for s in exporter.get_finished_spans()}


def test_sqlalchemy_queries_become_spans_once_enabled(spans):
    # 計装を有効にした後に作るエンジンが対象(`sqlalchemy.create_engine` を差し替えるため)
    engine = sqlalchemy.create_engine("sqlite://")
    with engine.connect() as conn:
        conn.execute(text("select 1"))
    assert any("sqlalchemy" in name for name in scopes(spans))


def test_sqlalchemy_is_not_instrumented_by_default():
    engine = sqlalchemy.create_engine("sqlite://")
    with engine.connect() as conn:
        assert conn.execute(text("select 1")).scalar() == 1


async def test_replay_client_calls_become_http_client_spans(spans):
    # 接続できないポートへ投げる(スパンはトランスポート層で作られ、接続失敗もスパンに残る)
    client = ReplayClient(base_url="http://127.0.0.1:1")
    with pytest.raises(ReplayUnavailable):
        await client.run("google/x", "p", timeout_secs=1)
    await client.aclose()
    assert any("httpx" in name for name in scopes(spans))


def test_get_engine_creates_instrumented_engines(spans):
    from shinasadame import db

    # db.py が `from sqlalchemy import create_engine` で参照を取っていると、ここで計装が効かない
    db.dispose_engines()
    with db.get_engine().connect() as conn:
        conn.execute(text("select 1"))
    assert any("sqlalchemy" in name for name in scopes(spans))


def test_replay_request_carries_traceparent_of_the_current_span(spans):
    """replay(Rust)が親として受け取れるよう、現在のスパンの `traceparent` がヘッダに載る。"""
    import asyncio
    import json
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    seen: dict[str, str | None] = {}

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802
            self.rfile.read(int(self.headers.get("Content-Length", 0)))
            seen["traceparent"] = self.headers.get("traceparent")
            body = json.dumps(
                {"response": "ok", "model": "m", "elapsed_ms": 1, "truncated": False}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):  # テスト出力を汚さない
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        from opentelemetry import trace

        async def call():
            client = ReplayClient(base_url=f"http://127.0.0.1:{server.server_port}")
            tracer = trace.get_tracer("test", tracer_provider=spans.provider)
            with tracer.start_as_current_span("parent") as parent:
                await client.run("google/x", "p", timeout_secs=5)
                await client.aclose()
                return format(parent.get_span_context().trace_id, "032x")

        trace_id = asyncio.run(call())
    finally:
        server.shutdown()
        thread.join(timeout=5)

    header = seen["traceparent"]
    assert header is not None, "traceparent ヘッダが送られていない"
    assert header.split("-")[1] == trace_id  # 同じトレースに属する


def test_healthz_is_excluded_from_fastapi_traces():
    from fastapi import FastAPI
    from fastapi.testclient import TestClient
    from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor

    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))

    probe_app = FastAPI()

    @probe_app.get("/healthz")
    def healthz():
        return "ok"

    @probe_app.get("/work")
    def work():
        return "done"

    telemetry.instrument_fastapi(probe_app, provider)
    try:
        with TestClient(probe_app) as client:
            assert client.get("/healthz").status_code == 200
            assert client.get("/work").status_code == 200
    finally:
        FastAPIInstrumentor.uninstrument_app(probe_app)

    names = [s.name for s in exporter.get_finished_spans()]
    assert any("/work" in n for n in names), names  # 通常のリクエストはスパンになる
    assert not any("/healthz" in n for n in names), names

