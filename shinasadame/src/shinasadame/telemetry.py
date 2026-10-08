"""SigNoz へのトレース送信。`OTEL_EXPORTER_OTLP_ENDPOINT` が未設定なら何もしない(ローカル開発・テスト)。"""

import logging

from fastapi import FastAPI

from . import config

log = logging.getLogger(__name__)


def setup_telemetry(app: FastAPI) -> bool:
    endpoint = config.otlp_endpoint()
    if not endpoint:
        return False

    from opentelemetry import trace
    from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
    from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor

    provider = TracerProvider(resource=Resource.create({"service.name": "shinasadame"}))
    provider.add_span_processor(
        BatchSpanProcessor(OTLPSpanExporter(endpoint=endpoint, insecure=not endpoint.startswith("https")))
    )
    trace.set_tracer_provider(provider)
    FastAPIInstrumentor.instrument_app(app, tracer_provider=provider)
    log.info("OpenTelemetry tracing enabled -> %s", endpoint)
    return True
