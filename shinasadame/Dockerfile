FROM python:3.12-slim

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv

WORKDIR /app
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy

# 依存だけ先に入れて、ソースの変更でレイヤーキャッシュが切れないようにする
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev --no-install-project

COPY src ./src
RUN uv sync --frozen --no-dev

ENV PATH="/app/.venv/bin:$PATH" \
    DATA_DIR=/data \
    PYTHONUNBUFFERED=1

# web と worker は同じイメージで、起動コマンドだけ変える(k8s/deployment.yaml)
CMD ["uvicorn", "shinasadame.app:app", "--host", "0.0.0.0", "--port", "8000"]
