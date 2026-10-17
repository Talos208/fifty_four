import httpx
import pytest

from shinasadame import config
from shinasadame.replay import ReplayClient, ReplayFailed, ReplayOk, ReplayUnavailable, replay_model_for


def client_with(handler) -> ReplayClient:
    return ReplayClient(base_url="http://replay.test", transport=httpx.MockTransport(handler))


async def test_run_returns_ok_and_sends_expected_request():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["path"] = request.url.path
        seen["body"] = request.read()
        return httpx.Response(
            200, json={"response": "返答", "model": "m", "elapsed_ms": 12, "truncated": False}
        )

    result = await client_with(handler).run("google/gemini-3.1-pro", "続きを書いて", timeout_secs=30)

    assert result == ReplayOk(response="返答", model="m", elapsed_ms=12, truncated=False)
    assert seen["path"] == "/v1/replay"
    import json

    assert json.loads(seen["body"]) == {
        "model": "google/gemini-3.1-pro",
        "prompt": "続きを書いて",
        "timeout_secs": 30,
    }


async def test_run_uses_default_timeout_when_not_given(monkeypatch):
    monkeypatch.setenv("REPLAY_DEFAULT_TIMEOUT_SECS", "45")
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        import json

        seen["timeout_secs"] = json.loads(request.read())["timeout_secs"]
        return httpx.Response(200, json={"response": "", "model": "m", "elapsed_ms": 1, "truncated": False})

    await client_with(handler).run("google/x", "p")
    assert seen["timeout_secs"] == 45


@pytest.mark.parametrize(
    ("status", "kind"),
    [(400, "bad_request"), (502, "auth"), (502, "llm"), (504, "timeout"), (500, "internal")],
)
async def test_run_maps_error_json_to_failed(status, kind):
    def handler(_request):
        return httpx.Response(status, json={"error": {"kind": kind, "message": "理由"}})

    result = await client_with(handler).run("google/x", "p")
    assert result == ReplayFailed(kind=kind, message="理由")


async def test_run_raises_unavailable_when_connection_fails():
    def handler(_request):
        raise httpx.ConnectError("refused")

    with pytest.raises(ReplayUnavailable):
        await client_with(handler).run("google/x", "p")


async def test_run_raises_unavailable_on_client_timeout():
    def handler(_request):
        raise httpx.ReadTimeout("slow")

    with pytest.raises(ReplayUnavailable):
        await client_with(handler).run("google/x", "p")


async def test_run_raises_unavailable_on_non_json_body():
    def handler(_request):
        return httpx.Response(502, text="<html>Bad Gateway</html>")

    with pytest.raises(ReplayUnavailable) as e:
        await client_with(handler).run("google/x", "p")
    assert "502" in str(e.value)


def test_config_defaults_and_model_choices(monkeypatch):
    monkeypatch.delenv("REPLAY_URL", raising=False)
    monkeypatch.delenv("REPLAY_DEFAULT_TIMEOUT_SECS", raising=False)
    monkeypatch.delenv("SHINASADAME_MODELS", raising=False)
    # 環境変数が無ければ models.json を使う。ここでは空のカタログにして「どちらも無ければ空」を確かめる
    monkeypatch.setattr(config.model_catalog, "load_catalog", lambda path=None: [])
    assert config.replay_url() == "http://replay:8080"
    assert config.replay_default_timeout_secs() == 120
    assert config.model_choices() == []

    monkeypatch.setenv("SHINASADAME_MODELS", " google/gemini-3.1-pro ,, anthropic/claude-sonnet-5-5 ,")
    assert config.model_choices() == ["google/gemini-3.1-pro", "anthropic/claude-sonnet-5-5"]


def test_config_ignores_invalid_timeout(monkeypatch):
    monkeypatch.setenv("REPLAY_DEFAULT_TIMEOUT_SECS", "abc")
    assert config.replay_default_timeout_secs() == 120


@pytest.mark.parametrize(
    "recorded, expected",
    [
        ("gemini-3.1-pro", "google/gemini-3.1-pro"),
        ("gpt-5.3", "openai/gpt-5.3"),
        ("claude-sonnet-5-5", "anthropic/claude-sonnet-5-5"),
        ("grok-4.7", "xai/grok-4.7"),
        ("google/gemini-3.1-pro", "google/gemini-3.1-pro"),
        ("lmstudio/qwen3.5-2b", "lmstudio/qwen3.5-2b"),
        ("lmstudio@h:1/m", "lmstudio@h:1/m"),
        ("cloudflare/@cf/meta/llama", "cloudflare/@cf/meta/llama"),
        ("qwen/qwen3-4b-2507", "lmstudio@host.docker.internal:1234/qwen/qwen3-4b-2507"),
        ("model-a", "lmstudio@host.docker.internal:1234/model-a"),
    ],
)
def test_replay_model_for(monkeypatch, recorded, expected):
    monkeypatch.delenv("REPLAY_LMSTUDIO_HOST", raising=False)
    assert replay_model_for(recorded) == expected


def test_replay_model_for_uses_lmstudio_host_override(monkeypatch):
    monkeypatch.setenv("REPLAY_LMSTUDIO_HOST", "10.0.0.5:4321")
    assert replay_model_for("qwen/qwen3-4b-2507") == "lmstudio@10.0.0.5:4321/qwen/qwen3-4b-2507"
