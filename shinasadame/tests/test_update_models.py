"""tools/update_models.py のテスト。ネットワークは使わず、小さな固定データで確かめる。"""

import update_models as um


def om(id, created, out=("text",), inp=("text",)):
    """OpenRouter の `data` 配列の要素。"""
    return {
        "id": id,
        "created": created,
        "architecture": {"input_modalities": list(inp), "output_modalities": list(out)},
    }


def chat(provider):
    return {"mode": "chat", "litellm_provider": provider}


def ids(models):
    return [m["id"] for m in models]


def test_selects_text_output_models_newest_first():
    openrouter = [
        om("anthropic/claude-haiku-5.5", 300),
        om("google/gemini-4-argon", 500),
        om("openai/gpt-6.1-sol", 450),
        om("x-ai/grok-4.7", 350),
        om("google/gemini-nano-banana-2.1", 400, out=("image", "text")),
    ]
    litellm = {
        "claude-haiku-5-5": chat("anthropic"),
        "gemini/gemini-4-argon": chat("gemini"),
        "gpt-6.1-sol": chat("openai"),
        "xai/grok-4.7": chat("xai"),
        "gemini/gemini-nano-banana-2.1": {
            "mode": "image_generation",
            "litellm_provider": "gemini",
            "supported_output_modalities": ["text", "image"],
        },
    }

    models, dropped = um.select_models(openrouter, litellm)

    assert ids(models) == [
        "google/gemini-4-argon",
        "openai/gpt-6.1-sol",
        "google/gemini-nano-banana-2.1",
        "xai/grok-4.7",
        "anthropic/claude-haiku-5-5",
    ]
    assert models[0]["created"] == 500
    assert dropped == []


def test_excludes_batch_and_free_variants():
    openrouter = [
        om("anthropic/claude-haiku-5.5", 300),
        om("anthropic/claude-haiku-5.5:batch", 300),
        om("anthropic/claude-haiku-5.5:free", 300),
    ]
    litellm = {"claude-haiku-5-5": chat("anthropic")}

    models, _ = um.select_models(openrouter, litellm)

    assert ids(models) == ["anthropic/claude-haiku-5-5"]


def test_excludes_non_text_output_and_embeddings_and_other_providers():
    openrouter = [
        om("google/lyria-3.5", 100, out=("audio",)),
        om("openai/text-embedding-9", 100),
        om("qwen/qwen3-max", 100),
        om("openai/gpt-6.1-sol", 100),
    ]
    litellm = {
        "gemini/lyria-3.5": chat("gemini"),
        "text-embedding-9": {"mode": "embedding", "litellm_provider": "openai"},
        "qwen3-max": chat("openai"),
        "gpt-6.1-sol": chat("openai"),
    }

    models, _ = um.select_models(openrouter, litellm)

    assert ids(models) == ["openai/gpt-6.1-sol"]


def test_drops_models_without_litellm_match_and_reports_them():
    openrouter = [om("openai/gpt-6.1-sol", 100), om("openai/gpt-9-unknown", 90)]
    litellm = {"gpt-6.1-sol": chat("openai")}

    models, dropped = um.select_models(openrouter, litellm)

    assert ids(models) == ["openai/gpt-6.1-sol"]
    assert dropped == ["openai/gpt-9-unknown"]


def test_drops_dated_snapshot_when_undated_twin_exists():
    openrouter = [
        om("anthropic/claude-sonnet-4.5", 100),
        om("anthropic/claude-sonnet-4.5-20250929", 90),
        om("anthropic/claude-old-20240101", 50),
    ]
    litellm = {
        "claude-sonnet-4-5": chat("anthropic"),
        "claude-sonnet-4-5-20250929": chat("anthropic"),
        "claude-old-20240101": chat("anthropic"),
    }

    models, _ = um.select_models(openrouter, litellm)

    # 日付付きは twin があれば消える。twin が無い日付付きは残る
    assert ids(models) == ["anthropic/claude-sonnet-4-5", "anthropic/claude-old-20240101"]


def test_name_overrides_map_mismatched_ids(monkeypatch):
    monkeypatch.setitem(um.NAME_OVERRIDES, "google/foo-x", "gemini/foo-y")
    openrouter = [om("google/foo-x", 100)]
    litellm = {"gemini/foo-y": chat("gemini")}

    models, dropped = um.select_models(openrouter, litellm)

    assert ids(models) == ["google/foo-y"]
    assert dropped == []


def test_latest_aliases_included_when_text_capable():
    litellm = {
        "gemini/gemini-flash-latest": chat("gemini"),
        "gemini/gemini-embedding-latest": {"mode": "embedding", "litellm_provider": "gemini"},
        "gemini/gemini-pro-latest": chat("gemini"),
        "gemini/gemini-4-argon": chat("gemini"),
    }

    aliases = um.latest_aliases(litellm, taken={"google/gemini-pro-latest"})

    assert aliases == [{"id": "google/gemini-flash-latest", "created": 0}]


# ---- run(): 取得・保存・失敗時の約束 ----

import json  # noqa: E402
import os  # noqa: E402

NOW = 1_790_000_000
OPENROUTER = [om("google/gemini-4-argon", 500), om("openai/gpt-6.1-sol", 450)]
LITELLM = {"gemini/gemini-4-argon": chat("gemini"), "gpt-6.1-sol": chat("openai")}


def fake_fetch(calls=None, openrouter=OPENROUTER, litellm=LITELLM):
    def fetch(url):
        if calls is not None:
            calls.append(url)
        return {"data": openrouter} if url == um.OPENROUTER_URL else litellm

    return fetch


def failing_fetch(url):
    raise OSError("network down")


def stamp(seconds):
    return um.time.strftime("%Y-%m-%dT%H:%M:%SZ", um.time.gmtime(seconds))


def write_existing(path, models, generated_at):
    path.write_text(json.dumps({"generated_at": generated_at, "models": models}), encoding="utf-8")


def test_run_writes_models_json(tmp_path):
    path = tmp_path / "models.json"

    code = um.run(path, fetch=fake_fetch(), now=lambda: NOW, force=True)

    data = json.loads(path.read_text(encoding="utf-8"))
    assert code == 0
    assert data["generated_at"] == stamp(NOW)
    assert data["models"] == [
        {"id": "google/gemini-4-argon", "created": 500},
        {"id": "openai/gpt-6.1-sol", "created": 450},
    ]


def test_run_keeps_existing_file_when_fetch_fails(tmp_path):
    path = tmp_path / "models.json"
    write_existing(path, [{"id": "google/old", "created": 1}], stamp(NOW - 10 * 86400))
    before = path.read_text(encoding="utf-8")

    code = um.run(path, fetch=failing_fetch, now=lambda: NOW)

    assert code == 0
    assert path.read_text(encoding="utf-8") == before


def test_run_creates_nothing_when_fetch_fails_and_no_file(tmp_path):
    path = tmp_path / "models.json"

    code = um.run(path, fetch=failing_fetch, now=lambda: NOW)

    assert code == 0
    assert not path.exists()


def test_run_does_not_overwrite_when_shrunk_by_more_than_half(tmp_path):
    path = tmp_path / "models.json"
    old = [{"id": f"openai/m{i}", "created": i} for i in range(10)]
    write_existing(path, old, stamp(NOW - 10 * 86400))
    before = path.read_text(encoding="utf-8")

    code = um.run(path, fetch=fake_fetch(), now=lambda: NOW)  # 新しい結果は 2 件

    assert code == 0
    assert path.read_text(encoding="utf-8") == before


def test_run_skips_fetch_when_fresh_unless_forced(tmp_path):
    path = tmp_path / "models.json"
    write_existing(path, [{"id": "google/old", "created": 1}], stamp(NOW - 3600))
    calls = []

    um.run(path, fetch=fake_fetch(calls), now=lambda: NOW)
    assert calls == []

    um.run(path, fetch=fake_fetch(calls), now=lambda: NOW, force=True)
    assert calls != []


def test_run_write_failure_keeps_existing_file_and_leaves_no_temp_files(tmp_path, monkeypatch):
    path = tmp_path / "models.json"
    write_existing(path, [{"id": "google/old", "created": 1}], stamp(NOW - 10 * 86400))
    before = path.read_text(encoding="utf-8")

    def broken_replace(src, dst):
        raise OSError("disk full")

    monkeypatch.setattr(os, "replace", broken_replace)

    code = um.run(path, fetch=fake_fetch(), now=lambda: NOW)

    assert code == 0
    assert path.read_text(encoding="utf-8") == before
    assert list(tmp_path.glob("models.json.*")) == []  # 一時ファイルが残らない


def test_is_sane():
    one = [{"id": "a/b", "created": 1}]
    assert not um.is_sane([], [])
    assert um.is_sane(one, [])
    assert um.is_sane(one * 5, one * 10)
    assert not um.is_sane(one * 4, one * 10)


def test_latest_aliases_has_no_duplicate_ids():
    # 実データでは `gemini/gemini-flash-latest` と接頭辞なしの `gemini-flash-latest` の両方がある
    litellm = {
        "gemini/gemini-flash-latest": chat("gemini"),
        "gemini-flash-latest": chat("gemini"),
    }

    aliases = um.latest_aliases(litellm, taken=set())

    assert aliases == [{"id": "google/gemini-flash-latest", "created": 0}]


# ---- 最終レビューの指摘: OpenAI の日付形式、廃止済みの除外 ----


def test_drops_openai_style_dated_snapshot_when_undated_twin_exists():
    openrouter = [om("openai/gpt-4o", 100), om("openai/gpt-4o-2024-08-06", 90)]
    litellm = {"gpt-4o": chat("openai"), "gpt-4o-2024-08-06": chat("openai")}

    models, _ = um.select_models(openrouter, litellm)

    assert ids(models) == ["openai/gpt-4o"]


def with_deprecation(entry, date):
    return {**entry, "deprecation_date": date}


def test_latest_aliases_skip_deprecated_entries():
    litellm = {
        "gpt-5.1-chat-latest": with_deprecation(chat("openai"), "2026-07-23"),  # 期限切れ
        "gpt-5.9-chat-latest": with_deprecation(chat("openai"), "2026-12-01"),  # まだ先
        "gpt-chat-latest": chat("openai"),  # 期限の情報なし
    }

    aliases = um.latest_aliases(litellm, taken=set(), today="2026-10-17")

    assert ids(aliases) == ["openai/gpt-5.9-chat-latest", "openai/gpt-chat-latest"]


def test_select_models_skips_deprecated_entries():
    openrouter = [om("openai/gpt-old", 100), om("openai/gpt-new", 90)]
    litellm = {
        "gpt-old": with_deprecation(chat("openai"), "2026-01-01"),
        "gpt-new": with_deprecation(chat("openai"), "2027-01-01"),
    }

    models, dropped = um.select_models(openrouter, litellm, today="2026-10-17")

    assert ids(models) == ["openai/gpt-new"]
    assert dropped == []  # 突き合わせ先が無いのではなく、廃止済みなので除外


def test_run_excludes_deprecated_aliases(tmp_path):
    path = tmp_path / "models.json"
    litellm = {
        **LITELLM,
        "gemini/gemini-old-latest": with_deprecation(chat("gemini"), "2020-01-01"),
        "gemini/gemini-flash-latest": chat("gemini"),
    }

    um.run(path, fetch=fake_fetch(litellm=litellm), now=lambda: NOW, force=True)

    written = [m["id"] for m in json.loads(path.read_text(encoding="utf-8"))["models"]]
    assert "google/gemini-flash-latest" in written
    assert "google/gemini-old-latest" not in written
