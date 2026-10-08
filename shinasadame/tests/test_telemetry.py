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
