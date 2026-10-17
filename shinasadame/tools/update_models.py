"""各社の「使えるモデル」を公開の一覧から集めて `models.json` を作る。標準ライブラリだけで動く。

取得元は OpenRouter(新しさと、テキストを出力できるかの判定)と LiteLLM(各社の正式な ID の確認)。
詳細は docs/superpowers/specs/2026-10-17-model-list-update-design.md。
"""

from __future__ import annotations

import argparse
import calendar
import json
import os
import re
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
LITELLM_URL = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
)
MODELS_PATH = Path(__file__).resolve().parent.parent / "src" / "shinasadame" / "models.json"
FRESH_SECONDS = 24 * 60 * 60  # これより新しい models.json があれば、取得しない(--force で上書き)
FETCH_TIMEOUT_SECONDS = 60
_TIME_FORMAT = "%Y-%m-%dT%H:%M:%SZ"

# OpenRouter の会社名 -> (出力する provider, OpenRouter の名前から LiteLLM のキーを作る関数)
PROVIDERS = {
    "google": ("google", lambda name: f"gemini/{name}"),
    "anthropic": ("anthropic", lambda name: name.replace(".", "-")),
    "openai": ("openai", lambda name: name),
    "x-ai": ("xai", lambda name: f"xai/{name}"),
}

# LiteLLM の litellm_provider -> 出力する provider(`-latest` の別名を拾うときに使う)
LITELLM_PROVIDERS = {"gemini": "google", "anthropic": "anthropic", "openai": "openai", "xai": "xai"}

# OpenRouter の ID -> LiteLLM のキー。綴りが食い違って自動では突き合わせられないモデルの、手で保守する対応表。
NAME_OVERRIDES: dict[str, str] = {}

_KEY_PREFIXES = ("gemini/", "xai/")
_DATED = re.compile(r"^(?P<base>.+)-(?:\d{8}|\d{4}-\d{2}-\d{2})$")  # Anthropic は YYYYMMDD、OpenAI は YYYY-MM-DD


def _native_name(litellm_key: str) -> str:
    """LiteLLM のキーから、会社を示す接頭辞(`gemini/` など)を除いた正式なモデル名。"""
    for prefix in _KEY_PREFIXES:
        if litellm_key.startswith(prefix):
            return litellm_key[len(prefix) :]
    return litellm_key


def _is_deprecated(entry: dict, today: str | None) -> bool:
    """LiteLLM の `deprecation_date`(YYYY-MM-DD)が、今日より前なら廃止済み。日付が無ければ判定できないので False。"""
    date = entry.get("deprecation_date")
    return bool(today and isinstance(date, str) and date < today)


def _is_text_model(entry: dict) -> bool:
    """LiteLLM の分類で、テキストを出力するモデルか。画像も出せるモデルは `image_generation` に分類される。"""
    mode = entry.get("mode")
    if mode == "chat":
        return True
    return mode == "image_generation" and "text" in (entry.get("supported_output_modalities") or [])


def select_models(openrouter: list[dict], litellm: dict, today: str | None = None) -> tuple[list[dict], list[str]]:
    """採用するモデルを新しい順に返す。2つ目は、LiteLLM に突き合わせ先が無くて落とした OpenRouter の ID。"""
    picked: dict[str, int] = {}
    dropped: list[str] = []
    for item in openrouter:
        openrouter_id = item["id"]
        if ":" in openrouter_id:  # `:batch` `:free` などは同じモデルの重複
            continue
        company, _, name = openrouter_id.partition("/")
        rule = PROVIDERS.get(company)
        if rule is None:
            continue
        arch = item.get("architecture") or {}
        if "text" not in (arch.get("output_modalities") or []) or "text" not in (
            arch.get("input_modalities") or []
        ):
            continue
        provider, to_key = rule
        key = NAME_OVERRIDES.get(openrouter_id) or to_key(name)
        entry = litellm.get(key)
        if not isinstance(entry, dict):
            dropped.append(openrouter_id)
            continue
        if not _is_text_model(entry) or _is_deprecated(entry, today):
            continue
        picked[f"{provider}/{_native_name(key)}"] = item.get("created", 0)

    # 日付付きの版は、日付なしの同じモデルがあれば載せない
    for model_id in list(picked):
        match = _DATED.match(model_id)
        if match and match.group("base") in picked:
            del picked[model_id]

    ordered = sorted(picked.items(), key=lambda pair: -pair[1])
    return [{"id": model_id, "created": created} for model_id, created in ordered], dropped


def latest_aliases(litellm: dict, taken: set[str], today: str | None = None) -> list[dict]:
    """LiteLLM にある `-latest` の別名(呼び出せる ID)。`taken` に既にあるものは除く。"""
    aliases = []
    for key, entry in litellm.items():
        if not isinstance(entry, dict) or not key.endswith("-latest"):
            continue
        provider = LITELLM_PROVIDERS.get(entry.get("litellm_provider"))
        if provider is None or not _is_text_model(entry) or _is_deprecated(entry, today):
            continue
        model_id = f"{provider}/{_native_name(key)}"
        if model_id not in taken:
            taken = taken | {model_id}  # 接頭辞あり・なしの両方のキーがあっても、1 回だけ
            aliases.append({"id": model_id, "created": 0})
    return aliases


def fetch_json(url: str) -> object:
    with urllib.request.urlopen(url, timeout=FETCH_TIMEOUT_SECONDS) as response:
        return json.loads(response.read().decode("utf-8"))


def is_sane(new: list[dict], old: list[dict]) -> bool:
    """新しい結果を採用してよいか。空は不可。既存があるなら、件数が半分未満に減っていたら壊れたとみなす。"""
    if not new:
        return False
    return not old or len(new) * 2 >= len(old)


def _read_existing(path: Path) -> dict | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or not isinstance(data.get("models"), list):
        return None
    return data


def _is_fresh(existing: dict, now: float) -> bool:
    try:
        generated = calendar.timegm(time.strptime(existing["generated_at"], _TIME_FORMAT))
    except (KeyError, TypeError, ValueError):
        return False
    return now - generated < FRESH_SECONDS


def _dumps(generated_at: str, models: list[dict]) -> str:
    """1 モデル 1 行にして、git の差分でモデルの増減を読みやすくする。"""
    lines = ",\n".join("  " + json.dumps(m, ensure_ascii=False) for m in models)
    return f'{{\n "generated_at": "{generated_at}",\n "models": [\n{lines}\n ]\n}}\n'


def _write_atomic(path: Path, text: str) -> None:
    """同じディレクトリの一時ファイルに書いてから置き換える。途中で失敗しても既存のファイルは壊れない。"""
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def run(path: Path, fetch=fetch_json, now=time.time, force: bool = False) -> int:
    """取得して `path` に書く。失敗しても警告だけで、戻り値は常に 0(ビルドを止めない)。"""
    existing = _read_existing(path)
    if existing and not force and _is_fresh(existing, now()):
        print(f"update_models: {path.name} は新しいので取得しない(--force で取得)")
        return 0
    old_models = existing["models"] if existing else []
    try:
        started = time.time()
        openrouter = fetch(OPENROUTER_URL)["data"]
        litellm = fetch(LITELLM_URL)
        today = time.strftime("%Y-%m-%d", time.gmtime(now()))
        models, dropped = select_models(openrouter, litellm, today)
        models += latest_aliases(litellm, {m["id"] for m in models}, today)
        if not is_sane(models, old_models):
            print(
                f"update_models: 警告: 結果が不自然(新 {len(models)} 件 / 既存 {len(old_models)} 件)なので上書きしない",
                file=sys.stderr,
            )
            return 0
        generated_at = time.strftime(_TIME_FORMAT, time.gmtime(now()))
        _write_atomic(path, _dumps(generated_at, models))
    except Exception as e:  # noqa: BLE001 - どんな失敗でもビルドは続ける
        print(f"update_models: 警告: 更新に失敗した(既存のファイルはそのまま): {e!r}", file=sys.stderr)
        return 0
    print(
        f"update_models: {len(models)} 件を書いた({len(dropped)} 件は LiteLLM に突き合わせ先が無く除外、"
        f"{time.time() - started:.1f} 秒)"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--force", action="store_true", help="新しい models.json があっても取得する")
    args = parser.parse_args(argv)
    return run(MODELS_PATH, force=args.force)


if __name__ == "__main__":
    sys.exit(main())
