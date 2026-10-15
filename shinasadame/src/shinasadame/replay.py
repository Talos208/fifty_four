"""replay サービス(`fifty_four_lsp --replay`)の HTTP クライアント。

HTTP ステータスの解釈はここに閉じ込める。呼び出し側は `ReplayOk` / `ReplayFailed` を見て、
サービスに届かなかったときだけ `ReplayUnavailable` を受け取る。
"""

from dataclasses import dataclass

import httpx

from . import config


_EXPLICIT_PREFIXES = ("google/", "openai/", "anthropic/", "xai/", "lmstudio/", "lmstudio@", "cloudflare/")
_NAME_PREFIXES = (("gemini-", "google"), ("gpt-", "openai"), ("claude-", "anthropic"), ("grok-", "xai"))


def replay_model_for(model_name: str) -> str:
    """記録されたモデル名(`get_model()` の素の名前)を replay が解釈できる文字列にする。

    provider 接頭辞が既にあればそのまま。既知の商用モデル名は provider を補い、
    それ以外は LM Studio のモデルとみなす(`qwen/qwen3-4b-2507` のような `/` 入りも含む)。
    """
    name = model_name.strip()
    if name.startswith(_EXPLICIT_PREFIXES):
        return name
    for prefix, provider in _NAME_PREFIXES:
        if name.startswith(prefix):
            return f"{provider}/{name}"
    return f"lmstudio@{config.lmstudio_host()}/{name}"


class ReplayUnavailable(Exception):
    """replay サービスに接続できない、または想定外の応答が返った。"""


@dataclass(frozen=True)
class ReplayOk:
    response: str
    model: str
    elapsed_ms: int
    truncated: bool


@dataclass(frozen=True)
class ReplayFailed:
    """replay サービスが返したエラー。kind は bad_request / auth / timeout / llm / internal。"""

    kind: str
    message: str


class ReplayClient:
    def __init__(
        self,
        base_url: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self._client = httpx.AsyncClient(base_url=base_url or config.replay_url(), transport=transport)

    async def aclose(self) -> None:
        await self._client.aclose()

    async def run(
        self, model: str, prompt: str, timeout_secs: int | None = None
    ) -> ReplayOk | ReplayFailed:
        timeout = timeout_secs or config.replay_default_timeout_secs()
        try:
            res = await self._client.post(
                "/v1/replay",
                json={"model": model, "prompt": prompt, "timeout_secs": timeout},
                # サーバー側のタイムアウトより少し長く待つ(サーバーの 504 を受け取れるように)
                timeout=timeout + 10,
            )
        except httpx.HTTPError as e:
            raise ReplayUnavailable(f"replay サービスに接続できません: {e!r}") from e

        try:
            body = res.json()
        except ValueError:
            raise ReplayUnavailable(f"replay サービスの応答を解釈できません (HTTP {res.status_code})") from None

        if res.status_code == 200 and isinstance(body, dict) and "response" in body:
            return ReplayOk(
                response=body["response"],
                model=body.get("model", model),
                elapsed_ms=int(body.get("elapsed_ms", 0)),
                truncated=bool(body.get("truncated", False)),
            )
        error = body.get("error") if isinstance(body, dict) else None
        if isinstance(error, dict) and "kind" in error:
            return ReplayFailed(kind=str(error["kind"]), message=str(error.get("message", "")))
        raise ReplayUnavailable(f"replay サービスの応答が想定外です (HTTP {res.status_code})")
