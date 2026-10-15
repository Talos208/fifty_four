"""SigNoz へのトレース送信。`OTEL_EXPORTER_OTLP_ENDPOINT` が未設定なら何もしない(ローカル開発・テスト)。"""

import logging

from fastapi import FastAPI

from . import config

log = logging.getLogger(__name__)

# k8s の readiness / liveness 用。トレースの対象外(excluded_urls は正規表現で URL を検索する)
HEALTH_PATH = "/healthz"


def setup_telemetry(app: FastAPI) -> bool:
    endpoint = config.otlp_endpoint()
    if not endpoint:
        return False

    from opentelemetry import trace
    from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor

    provider = TracerProvider(resource=Resource.create({"service.name": "shinasadame"}))
    provider.add_span_processor(
        BatchSpanProcessor(OTLPSpanExporter(endpoint=endpoint, insecure=not endpoint.startswith("https")))
    )
    trace.set_tracer_provider(provider)
    instrument_fastapi(app, provider)
    instrument_clients(provider)
    log.info("OpenTelemetry tracing enabled -> %s", endpoint)
    return True


def instrument_fastapi(app: FastAPI, provider) -> None:  # noqa: ANN001
    """受信側(FastAPI)の計装。k8s のプローブ(/healthz)は頻繁に来るのでトレースから外す。"""
    from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor

    FastAPIInstrumentor.instrument_app(app, tracer_provider=provider, excluded_urls=HEALTH_PATH)


def instrument_clients(provider) -> None:  # noqa: ANN001
    """送信側の計装を有効にする。

    - httpx: replay サービスへの呼び出しが CLIENT スパンになり、`traceparent` ヘッダも付く。
      計装はプロセス全体(トランスポートのクラス)に効くので、後から作るクライアントにも効く。
    - SQLAlchemy: `sqlalchemy.create_engine` を差し替えるので、これより後に作るエンジンが対象になる。
      db.py はエンジンを `sqlalchemy.create_engine(...)` で遅延して作る(import 時に参照を取らない)。
    """
    from opentelemetry.instrumentation.httpx import HTTPXClientInstrumentor
    from opentelemetry.instrumentation.sqlalchemy import SQLAlchemyInstrumentor

    HTTPXClientInstrumentor().instrument(tracer_provider=provider)
    SQLAlchemyInstrumentor().instrument(tracer_provider=provider)
