import json

import pytest

from shinasadame import config, model_catalog
from shinasadame.model_catalog import ProviderGroup, Series, group_models, load_catalog, series_of


@pytest.mark.parametrize(
    ("model_id", "expected"),
    [
        ("anthropic/claude-haiku-5-5", "haiku"),
        ("anthropic/claude-fable-5-1", "fable"),
        ("anthropic/claude-3-5-sonnet-20241022", "sonnet"),  # 版が先の旧名
        ("google/gemini-3.8-flash", "gemini-3.8"),
        ("google/gemini-4-argon", "gemini-4"),
        ("google/gemma-4-31b-it", "gemma-4"),
        ("google/gemini-flash-latest", None),
        ("openai/gpt-6.1-sol", "gpt-6.1"),
        ("openai/gpt-6-luna", "gpt-6"),
        ("openai/gpt-4o-mini", "gpt-4o"),
        ("openai/o3", None),
        ("xai/grok-4.7", "grok-4.7"),
        ("xai/grok-build-0.1", None),
        ("mistral/foo", None),
    ],
)
def test_series_of_examples(model_id, expected):
    assert series_of(model_id) == expected


def test_group_models_orders_providers_and_keeps_input_order():
    groups = group_models(
        [
            "xai/grok-4.7",
            "openai/gpt-6.1-sol",
            "google/gemini-4-argon",
            "openai/gpt-6-luna",
            "anthropic/claude-haiku-5-5",
            "openai/gpt-6.1-sol-mini",
            "mistral/foo",
        ]
    )

    assert [g.provider for g in groups] == ["google", "anthropic", "openai", "xai", "mistral"]
    openai = groups[2]
    # 系列は最初に現れた順、モデルは入力の順
    assert openai == ProviderGroup(
        provider="openai",
        series=[
            Series(label="gpt-6.1", models=["openai/gpt-6.1-sol", "openai/gpt-6.1-sol-mini"]),
            Series(label="gpt-6", models=["openai/gpt-6-luna"]),
        ],
    )


def test_group_models_puts_unseriesable_directly_under_provider():
    groups = group_models(["openai/gpt-6.1-sol", "openai/o3"])

    assert groups[0].series == [
        Series(label="gpt-6.1", models=["openai/gpt-6.1-sol"]),
        Series(label=None, models=["openai/o3"]),
    ]


def test_group_models_of_nothing_is_empty():
    assert group_models([]) == []


def test_load_catalog_reads_ids_in_order(tmp_path):
    path = tmp_path / "models.json"
    path.write_text(
        json.dumps({"generated_at": "x", "models": [{"id": "b/x", "created": 2}, {"id": "a/y", "created": 1}]}),
        encoding="utf-8",
    )

    assert load_catalog(path) == ["b/x", "a/y"]


@pytest.mark.parametrize("content", [None, "{broken", "[]", '{"models": "nope"}', '{"nomodels": 1}', '{"models": [1, {"x": 2}]}'])
def test_load_catalog_returns_empty_for_missing_or_broken_file(tmp_path, content):
    path = tmp_path / "models.json"
    if content is not None:
        path.write_text(content, encoding="utf-8")

    assert load_catalog(path) == []


def test_model_choices_prefers_env_over_catalog(monkeypatch):
    monkeypatch.setattr(model_catalog, "load_catalog", lambda path=None: ["google/from-catalog"])

    monkeypatch.setenv("SHINASADAME_MODELS", " a/one ,, b/two ,")
    assert config.model_choices() == ["a/one", "b/two"]

    monkeypatch.delenv("SHINASADAME_MODELS")
    assert config.model_choices() == ["google/from-catalog"]


@pytest.mark.parametrize(
    ("model_id", "expected"),
    [
        ("google/gemini-4-argon", True),
        ("mistral/foo", True),
        ("lmstudio@host.docker.internal:1234/model-a", False),  # LM Studio は会社ではない
        ("no-slash", False),
        ("/leading", False),
    ],
)
def test_is_provider_model(model_id, expected):
    assert model_catalog.is_provider_model(model_id) is expected


def test_group_models_skips_non_provider_ids():
    groups = group_models(["lmstudio@host:1234/model-a", "google/gemini-4-argon"])

    assert [g.provider for g in groups] == ["google"]
