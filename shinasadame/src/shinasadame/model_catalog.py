"""「試す」パネルのモデル選択肢。`models.json`(tools/update_models.py が作る)を読み、会社・系列にまとめる。

系列は表示用のまとまり。フォームに送られる値は、系列に関係なく `provider/model` のまま。
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

MODELS_JSON = Path(__file__).resolve().parent / "models.json"

# 会社の表示順。ここに無い会社は、この後に名前順で並べる。
PROVIDER_ORDER = ("google", "anthropic", "openai", "xai")

# 会社ごとの系列の切り方。名前(provider/ を除いた部分)に当てて、系列のラベルを作る。
_ANTHROPIC_FAMILY = re.compile(r"(haiku|sonnet|opus|fable)")  # 版が先の旧名(claude-3-5-sonnet)にも当たる
_GOOGLE_VERSION = re.compile(r"^(gemini|gemma)-(\d+(?:\.\d+)?)")
_OPENAI_VERSION = re.compile(r"^gpt-(\d+(?:\.\d+)?o?)")  # gpt-4o は 4o まで
_XAI_VERSION = re.compile(r"^grok-(\d+(?:\.\d+)?)")

# `provider/model` の provider の形。`lmstudio@host:1234/model` のようなものは会社ではないので除く。
_PROVIDER = re.compile(r"^[a-z0-9][a-z0-9_.-]*$")


@dataclass(frozen=True)
class Series:
    """メニューの 1 段。`label` が None のモデルは、会社の直下に並べる。"""

    label: str | None
    models: list[str]


@dataclass(frozen=True)
class ProviderGroup:
    provider: str
    series: list[Series]


def load_catalog(path: Path | None = None) -> list[str]:
    """`models.json` のモデル ID を、ファイルの並び順(新しい順)のまま返す。

    ファイルが無い・壊れている・形が違うときは空(画面は自由入力欄だけで使える)。
    """
    try:
        data = json.loads((path or MODELS_JSON).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, list):
        return []
    return [m["id"] for m in models if isinstance(m, dict) and isinstance(m.get("id"), str)]


def is_provider_model(model_id: str) -> bool:
    """`provider/model` の形(会社名で始まる)か。LM Studio の `lmstudio@…/model` などは False。"""
    provider, slash, name = model_id.partition("/")
    return bool(slash and name and _PROVIDER.match(provider))


def series_of(model_id: str) -> str | None:
    """`provider/model` の系列ラベル。切り出せなければ None。"""
    provider, _, name = model_id.partition("/")
    if provider == "anthropic":
        match = _ANTHROPIC_FAMILY.search(name)
        return match.group(1) if match else None
    if provider == "google":
        match = _GOOGLE_VERSION.match(name)
        return f"{match.group(1)}-{match.group(2)}" if match else None
    if provider == "openai":
        match = _OPENAI_VERSION.match(name)
        return f"gpt-{match.group(1)}" if match else None
    if provider == "xai":
        match = _XAI_VERSION.match(name)
        return f"grok-{match.group(1)}" if match else None
    return None


def group_models(model_ids: list[str]) -> list[ProviderGroup]:
    """会社 ▸ 系列 ▸ モデルにまとめる。系列とモデルは、入力で最初に現れた順を保つ。

    `provider/` の形でない ID(`lmstudio@…` など。`is_provider_model`)と、重複した ID は扱わない。
    """
    by_provider: dict[str, dict[str | None, list[str]]] = {}
    seen: set[str] = set()
    for model_id in model_ids:
        if not is_provider_model(model_id) or model_id in seen:
            continue
        seen.add(model_id)
        provider = model_id.partition("/")[0]
        by_provider.setdefault(provider, {}).setdefault(series_of(model_id), []).append(model_id)

    known = [p for p in PROVIDER_ORDER if p in by_provider]
    others = sorted(p for p in by_provider if p not in PROVIDER_ORDER)
    return [
        ProviderGroup(provider, [Series(label, models) for label, models in by_provider[provider].items()])
        for provider in known + others
    ]
