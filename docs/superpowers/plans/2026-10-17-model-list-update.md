# モデル一覧の自動更新 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 各社の「使えるモデル」を公開の一覧から自動取得して `models.json` に保存し、「試す」パネルのモデル欄を複数段のメニュー(会社 ▸ 系列 ▸ モデル)にする。

**Architecture:** `shinasadame/tools/update_models.py`(標準ライブラリのみ)が OpenRouter と LiteLLM の公開 JSON から一覧を作り、`shinasadame/src/shinasadame/models.json` に書く。アプリ側の `model_catalog.py` がそれを読み、会社・系列でまとめる。`record_detail.html` は `<select name="model">`(`<optgroup>`付き)を描画し、`app.js` が複数段メニューに差し替える(JS が無ければ select のまま使える)。スクリプトは Skaffold の `shinasadame` イメージのビルド前フックで回る。

**Tech Stack:** Python 3.11 + uv、FastAPI + Jinja2 + htmx、素の JavaScript / CSS、pytest、Skaffold

**Spec:** `docs/superpowers/specs/2026-10-17-model-list-update-design.md`

## Global Constraints

- **コミットしない。** 各タスクにコミット手順は無い。コミットは、利用者が指示したときだけ(`git add` もしない)。サブエージェントにも明示すること。
- 依存の追加なし。スクリプトは標準ライブラリのみ。JS のビルド工程は作らない。Python 3.11 以上、実行は `uv run`。
- モデルの指定形式は `provider/model`、provider は `google` / `anthropic` / `openai` / `xai`。フォームは単一の `model` フィールドのまま(バックエンド `try_prompt` は変えない)。
- 取得元: OpenRouter `https://openrouter.ai/api/v1/models`(キー不要)と、LiteLLM `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`。各社の公式ページは使わない。
- 取得・整形に失敗しても、ビルドを止めない(終了コード 0)。既存の `models.json` は残す。新しい件数が既存の半分未満なら上書きしない。
- テストはネットワークに依存しない。`acceptance.html` のモデル絞り込みには触らない。
- 秘密や API キーは扱わない。

## データの事実(2026-10-17 の実物で確認済み。ビルダーは再確認すること)

- OpenRouter の各要素: `id`(例 `anthropic/claude-haiku-5.5`)、`created`(UNIX 秒)、`architecture.input_modalities` / `output_modalities`(list)。`:batch` `:free` 付きの重複 ID がある。
- LiteLLM のキー: Anthropic は `claude-haiku-5-5`、OpenAI は `gpt-6.1-sol`、Google は `gemini/gemini-4-argon`、xAI は `xai/grok-4.7`。各値に `mode`(`chat` / `image_generation` / `embedding` …)と、あれば `supported_output_modalities`。
- `google/gemini-nano-banana-2.1` は OpenRouter で出力 `['image','text']`、LiteLLM で `mode: image_generation`・`supported_output_modalities: ['text','image']`。**これは通す。**
- 既存テスト `test_trial.py` は `<select name="model"`、`<option value="X" selected>`(既定値、属性はこれだけ)、`<option value="google/…"`(前方一致)、既定値の option が1つだけであること、を検証している。

## Review Focus

- `:batch` / `:free` の重複 ID が一覧に混ざらない(Task 1)。
- 日付付きの版が、日付なしの同じモデルと並んで出ない(Task 1)。
- 取得に失敗したら、既存ファイルのまま・終了コード 0(Task 2)。
- 件数が極端に減ったら上書きしない(Task 2)。
- `models.json` が無い・壊れている・空でも、画面が 500 にならず自由入力で使える(Task 3, 4)。
- 記録のモデル(既定値)が一覧に無くても、先頭に選択済みで出る(Task 4)。

## ファイル構成

| ファイル | 責務 |
|---|---|
| `shinasadame/tools/update_models.py`(新規) | 取得・整形・保存。純粋関数と I/O を分ける |
| `shinasadame/src/shinasadame/models.json`(生成物) | 取得結果。コミット対象(コミットは利用者) |
| `shinasadame/src/shinasadame/model_catalog.py`(新規) | `models.json` の読み込みと、会社・系列へのまとめ |
| `shinasadame/src/shinasadame/config.py`(変更) | `model_choices()` を、環境変数 → カタログの順に |
| `shinasadame/src/shinasadame/app.py` / `templates/record_detail.html`(変更) | `<optgroup>` 付き select の描画 |
| `shinasadame/src/shinasadame/static/app.js` / `style.css`(変更) | 複数段メニュー |
| `shinasadame/tests/test_update_models.py` / `test_model_catalog.py`(新規) | 上記のテスト |
| `shinasadame/pyproject.toml`(変更) | pytest の `pythonpath` に `tools` を追加 |
| `skaffold.yaml` / `shinasadame/k8s/base/deployment.yaml` / `shinasadame/README.md`(変更) | フック、環境変数の削除、手順 |

---

### Task 1: 取得結果の整形(純粋関数)

**Files:**
- Create: `shinasadame/tools/update_models.py`
- Create: `shinasadame/tests/test_update_models.py`
- Modify: `shinasadame/pyproject.toml`(`[tool.pytest.ini_options] pythonpath = ["tests", "tools"]`)

**Interfaces:**
- Produces(Task 2 が使う):
  - `NAME_OVERRIDES: dict[str, str]` — OpenRouter の ID → LiteLLM のキー。綴りが食い違うモデルの手動対応表(初期は空。実データで食い違いが見つかれば足す)
  - `def select_models(openrouter: list[dict], litellm: dict) -> tuple[list[dict], list[str]]` — 採用モデルを `[{"id": "google/gemini-4-argon", "created": 1791000000}, ...]`(`created` の降順)で、落としたモデルの OpenRouter ID を2番目で返す。`openrouter` は `data` 配列そのもの
  - `def latest_aliases(litellm: dict, taken: set[str]) -> list[dict]` — `-latest` の別名を `{"id", "created": 0}` で返す

- [ ] **Step 1: 失敗するテストを書く**(`tests/test_update_models.py`)。小さな固定データ(`om(id, created, out=("text",), inp=("text",))` で OpenRouter 要素を作るヘルパ)で次を検証する。
  - `test_selects_text_output_models_newest_first`: 入力 = `anthropic/claude-haiku-5.5`(300)、`google/gemini-4-argon`(500)、`openai/gpt-6.1-sol`(450)、`x-ai/grok-4.7`(350)、`google/gemini-nano-banana-2.1`(400, 出力 `image,text`)。LiteLLM = `claude-haiku-5-5` / `gemini/gemini-4-argon` / `gpt-6.1-sol` / `xai/grok-4.7`(いずれも `mode: chat`)と `gemini/gemini-nano-banana-2.1`(`mode: image_generation`, `supported_output_modalities: [text, image]`)。期待する ID の並び = `["google/gemini-4-argon", "openai/gpt-6.1-sol", "google/gemini-nano-banana-2.1", "xai/grok-4.7", "anthropic/claude-haiku-5-5"]`
  - `test_excludes_batch_and_free_variants`: `anthropic/claude-haiku-5.5:batch`、`…:free` を入れても結果に出ない
  - `test_excludes_non_text_output_and_embeddings_and_other_providers`: 出力が `audio` のみ、LiteLLM の `mode: embedding`、`qwen/…`(対象外の会社)は結果に出ない
  - `test_drops_models_without_litellm_match_and_reports_them`: LiteLLM に無いモデルは結果に無く、2番目の戻り値にその OpenRouter ID が入る
  - `test_drops_dated_snapshot_when_undated_twin_exists`: `claude-sonnet-4-5` と `claude-sonnet-4-5-20250929` が両方とも採用候補なら、日付付きは出ない(日付だけがあり twin が無ければ残る)
  - `test_name_overrides_map_mismatched_ids`: `NAME_OVERRIDES` に `{"google/foo-x": "gemini/foo-y"}` を monkeypatch で入れると、`google/foo-y` として採用される(出力 ID は LiteLLM 側の名前から作る)
  - `test_latest_aliases_included_when_text_capable`: LiteLLM の `gemini/gemini-flash-latest`(`mode: chat`)が `{"id": "google/gemini-flash-latest", "created": 0}` で返り、`mode: embedding` のものや `taken` に既にあるものは返らない

- [ ] **Step 2: 失敗を確認する** — Run: `cd shinasadame && uv run pytest tests/test_update_models.py -v` / Expected: `ModuleNotFoundError: update_models`

- [ ] **Step 3: 実装する** — `select_models` / `latest_aliases` / `NAME_OVERRIDES`(`tools/update_models.py`)。要点: OpenRouter の ID を `/` で会社と名前に割り、会社 → 出力 provider の対応は `google→google` `anthropic→anthropic` `openai→openai` `x-ai→xai`。LiteLLM のキーは `google→"gemini/"+name`、`anthropic→name.replace(".", "-")`、`openai→name`、`xai→"xai/"+name`(`NAME_OVERRIDES` が先)。出力 ID は `provider/` + LiteLLM のキーから会社接頭辞を除いた名前。採用条件は、OpenRouter の `output_modalities` に `text` を含み、`input_modalities` にも `text` を含み、LiteLLM の `mode == "chat"` または `supported_output_modalities` に `text` を含むこと。`:` を含む OpenRouter ID は除く。日付付き(`-YYYYMMDD` で終わる)は、その接尾辞を除いた名前が採用集合にあれば除く。

- [ ] **Step 4: 通ることを確認する** — Run: `cd shinasadame && uv run pytest tests/test_update_models.py -v` / Expected: 全て PASS

- [ ] **Step 5: 実データで食い違いを確認する(読み取りだけ)** — OpenRouter と LiteLLM を実際に取得し、`select_models` の2番目の戻り値(落ちたモデル)を眺める。4社の最新(`gemini-4-argon`、`claude-haiku-5-5`、`gpt-6.1-sol`、`grok-4.7`)が採用に入っていること、会社ごとのルールで救える食い違いが `NAME_OVERRIDES` に入っていること。結果を作業メモとして報告に書く(ファイルはまだ書かない)。

---

### Task 2: 取得・保存・失敗時の約束(I/O)

**Files:**
- Modify: `shinasadame/tools/update_models.py`
- Modify: `shinasadame/tests/test_update_models.py`

**Interfaces:**
- Consumes: Task 1 の `select_models` / `latest_aliases`
- Produces:
  - `def is_sane(new: list[dict], old: list[dict]) -> bool` — `new` が空でなく、`old` があるなら `len(new) * 2 >= len(old)`
  - `def run(path: Path, fetch=fetch_json, now=time.time, force: bool = False) -> int` — 常に `0` を返す(失敗しても)。`fetch(url) -> object` は差し替え可能
  - `models.json` の形式: `{"generated_at": "2026-10-17T12:00:00Z", "models": [{"id": "google/gemini-4-argon", "created": 1791000000}, ...]}`(`models` は新しい順、`-latest` の別名は末尾)
  - コマンド: `uv run python tools/update_models.py [--force]`。出力先は `src/shinasadame/models.json`(スクリプトの位置から解決する)

- [ ] **Step 1: 失敗するテストを書く**(`fetch` を差し替える。ネットワークは使わない)
  - `test_run_writes_models_json`: 正常な fetch で `models.json` が上の形式で書かれ、戻り値は 0
  - `test_run_keeps_existing_file_when_fetch_fails`: fetch が例外を出す → ファイルの中身は変わらず、戻り値は 0
  - `test_run_creates_nothing_when_fetch_fails_and_no_file`: ファイルが無く fetch が失敗 → ファイルは作られず、戻り値は 0
  - `test_run_does_not_overwrite_when_shrunk_by_more_than_half`: 既存 10 件で新しい結果が 4 件 → 上書きされない
  - `test_run_skips_fetch_when_fresh`: `generated_at` が 24 時間以内 → fetch は呼ばれない。`force=True` なら呼ばれる
  - `test_run_write_is_atomic`: 書き込みは一時ファイルを経由する(書き込み中の例外でも、既存ファイルが壊れない)

- [ ] **Step 2: 失敗を確認する** — Run: `cd shinasadame && uv run pytest tests/test_update_models.py -v` / Expected: 新しいテストが FAIL

- [ ] **Step 3: 実装する** — `fetch_json(url)`(`urllib.request`、タイムアウト 60 秒、UTF-8 で JSON をパース)、`is_sane`、`run`、`main()`(`--force` 対応)。失敗(通信・JSON・形の想定外)は、警告を標準エラーに出して `0` を返す。保存は、同じディレクトリに一時ファイルを書いてから置き換える。件数・落ちた件数・所要時間を標準出力に一行で出す。

- [ ] **Step 4: 通ることを確認する** — Run: `cd shinasadame && uv run pytest tests/test_update_models.py -v` / Expected: 全て PASS

- [ ] **Step 5: 実機で種を作る** — Run: `cd shinasadame && uv run python tools/update_models.py --force` / Expected: `src/shinasadame/models.json` ができ、4社の最新が入っている。ネットワークを切った状態(`HTTPS_PROXY` を存在しないアドレスにするなど)で再実行して、警告が出て、ファイルが変わらず、終了コードが 0 であることも確認する。**生成されたファイルはコミットしない**(利用者がコミットする)。

---

### Task 3: カタログの読み込みと、会社・系列へのまとめ

**Files:**
- Create: `shinasadame/src/shinasadame/model_catalog.py`
- Create: `shinasadame/tests/test_model_catalog.py`
- Modify: `shinasadame/src/shinasadame/config.py`(`model_choices`)

**Interfaces:**
- Consumes: Task 2 の `models.json` の形式
- Produces(Task 4 が使う):
  - `@dataclass(frozen=True) class Series: label: str | None; models: list[str]` — `label` が `None` なら会社の直下
  - `@dataclass(frozen=True) class ProviderGroup: provider: str; series: list[Series]`
  - `def load_catalog(path: Path | None = None) -> list[str]` — `models.json` の ID を並び順のまま。無い・壊れている・形が違うときは `[]`(例外にしない)
  - `def series_of(model_id: str) -> str | None` — 会社ごとの系列(下記)
  - `def group_models(model_ids: list[str]) -> list[ProviderGroup]` — 会社は `google, anthropic, openai, xai` の順、その他は名前順。系列は最初に現れた順。`provider/` の形でない ID(例 `lmstudio@host/model`)は扱わない(呼び出し側で除く)
  - `config.model_choices() -> list[str]` — 環境変数 `SHINASADAME_MODELS` が設定されていればそれ(今までどおり)、無ければ `load_catalog()`

系列の切り方(初版): Anthropic = 家族名(`claude-haiku-5-5` → `haiku`、`claude-3-5-sonnet-…` のような版が先の旧名も `sonnet`)、Google = `gemini-N.M`(`gemini-3.8-flash` → `gemini-3.8`、`gemini-4-argon` → `gemini-4`)、OpenAI = `gpt-N` または `gpt-N.M`(`gpt-6.1-sol` → `gpt-6.1`、`gpt-6-luna` → `gpt-6`)、xAI = `grok-N` または `grok-N.M`(`grok-4.7` → `grok-4.7`)。切れないものは `None`。

- [ ] **Step 1: 失敗するテストを書く**
  - `test_series_of_examples`: 上の例(`anthropic/claude-haiku-5-5`→`haiku`、`google/gemini-3.8-flash`→`gemini-3.8`、`google/gemini-4-argon`→`gemini-4`、`openai/gpt-6.1-sol`→`gpt-6.1`、`openai/gpt-6-luna`→`gpt-6`、`xai/grok-4.7`→`grok-4.7`、`openai/o3`→`None`)
  - `test_group_models_orders_providers_and_keeps_input_order`: 入力の順序を保ち、会社が `google, anthropic, openai, xai` の順に並ぶ
  - `test_group_models_puts_unseriesable_directly_under_provider`: `series_of` が `None` のモデルは `Series(label=None, ...)` に入る
  - `test_load_catalog_returns_empty_for_missing_or_broken_file`: 存在しないパス、壊れた JSON、`models` が無い JSON のそれぞれで `[]`
  - `test_model_choices_prefers_env_over_catalog`: 環境変数があればそれ、無ければカタログ(`load_catalog` を monkeypatch)

- [ ] **Step 2: 失敗を確認する** — Run: `cd shinasadame && uv run pytest tests/test_model_catalog.py -v` / Expected: FAIL

- [ ] **Step 3: 実装する** — `model_catalog.py` の上記と `config.model_choices`。`models.json` の既定の場所は、このモジュールと同じディレクトリ。

- [ ] **Step 4: 通ることを確認する** — Run: `cd shinasadame && uv run pytest tests/test_model_catalog.py tests/test_replay_client.py -v` / Expected: 全て PASS(`test_replay_client.py` の `model_choices` のテストが通り続けること)

---

### Task 4: 画面(サーバー側): `<optgroup>` 付き select

**Files:**
- Modify: `shinasadame/src/shinasadame/app.py`(`record_detail`)
- Modify: `shinasadame/src/shinasadame/templates/record_detail.html`
- Modify: `shinasadame/tests/test_trial.py`

**Interfaces:**
- Consumes: Task 3 の `group_models` / `config.model_choices`
- Produces(Task 5 が使う): HTML の約束 — `<select name="model" required data-cascade>`。先頭に既定値の `<option value="X" selected>X</option>`(属性は `value` と `selected` だけ)。続いて会社ごとの `<optgroup label="google">`、その中に `<option value="google/gemini-4-argon" data-series="gemini-4">…</option>`(`data-series` は系列が `None` なら属性なし)

- [ ] **Step 1: 失敗するテストを書く**(`test_trial.py` に追加。既存の4つのモデル関連テストは変えずに通ること)
  - `test_model_select_groups_options_by_provider_with_series`: 環境変数 `SHINASADAME_MODELS="google/gemini-4-argon,openai/gpt-6.1-sol"` で、ページに `<optgroup label="google">` と `<optgroup label="openai">` があり、`<option value="openai/gpt-6.1-sol" data-series="gpt-6.1">` がある。`data-cascade` が select にある
  - `test_model_select_uses_catalog_when_env_unset`: 環境変数を外し、`config.model_choices` を monkeypatch して2件返すと、それが選択肢に並ぶ
  - `test_page_renders_when_catalog_is_empty`: 環境変数なし・カタログ空(`[]`)でも 200 で、既定値の option と `name="model_custom"` がある
  - `test_default_model_not_in_catalog_is_first_and_selected`: 既存テストが担保している内容に加え、カタログの option に既定値が重複しない(既存の `count == 1` テストが通ること)

- [ ] **Step 2: 失敗を確認する** — Run: `cd shinasadame && uv run pytest tests/test_trial.py -v` / Expected: 新しいテストが FAIL

- [ ] **Step 3: 実装する** — `record_detail` が `model_options`(今の平らな一覧)のかわりに `default_model` と `model_groups=group_models([...既定値を除く、provider/ 形の候補])` をテンプレートに渡す。`provider/` の形でない候補(`lmstudio@…` など)は、既定値でなければ select から外す代わりに、`<optgroup label="その他">` に入れる。テンプレートは上の HTML の約束どおりに描画する。

- [ ] **Step 4: 通ることを確認する** — Run: `cd shinasadame && uv run pytest -q` / Expected: 全て PASS(shinasadame の全テスト)

---

### Task 5: 複数段メニュー(JS / CSS)

**Files:**
- Modify: `shinasadame/src/shinasadame/static/app.js`
- Modify: `shinasadame/src/shinasadame/static/style.css`

**Interfaces:**
- Consumes: Task 4 の HTML の約束(`select[data-cascade]`、`optgroup`、`option[data-series]`)
- Produces: `select[data-cascade]` を、ボタン + 入れ子のメニュー(会社 ▸ 系列 ▸ モデル)に差し替える。元の select は残して非表示にし、選んだ値を `select.value` に入れ、`change` イベントを発火する(フォームはそのまま送れる)。JS が無効なら select のまま。

挙動の要件: ボタンに現在のモデルを出す。クリック / Enter / Space で開く。会社に乗せる(ホバー or → キー)と系列、系列に乗せるとモデルが横に開く。系列が `None` のモデルは会社の直下に出す。↑↓で移動、→で子を開く、←と Esc で閉じる、メニューの外のクリックで閉じる。メニューが画面の右端からはみ出す場合は、左に開く。選んだら閉じる。`details` の中(「プロンプトとモデルを編集する」)に置かれるので、`details` を閉じても壊れない。

- [ ] **Step 1: 実装する** — 上の挙動。既存の `app.js` の書き方(即時実行ではなく、トップレベルの関数と `DOMContentLoaded`)に合わせる。htmx で差し替わる断片ではなく、ページ読み込み時に一度だけ初期化する。

- [ ] **Step 2: ブラウザで確認する**(JS のテスト基盤は無いので、実機で見る) — アプリを起動し(`cd shinasadame && uv run uvicorn shinasadame.app:app`、データは `DATA_DIR` を一時ディレクトリに向ける)、Chrome DevTools で、補完レコードの詳細ページを開く。確認する観察: ①ボタンが現在のモデルを表示 ②開くと会社が並ぶ ③会社に乗せると系列、系列に乗せるとモデルが横に開く ④選ぶと `select.value` が変わり、「試す」の送信で選んだモデルが `model` として送られる ⑤キーボード(↑↓→←Esc)で操作できる ⑥ウィンドウ幅を狭めても、メニューが画面外に出ない ⑦ JS を無効にすると、`optgroup` 付きの select で選べる。確認したら、起動したサーバーを必ず止める。

---

### Task 6: Skaffold のフックと、環境変数の整理

**Files:**
- Modify: `skaffold.yaml`(`shinasadame` の `artifacts` にビルド前フック)
- Modify: `shinasadame/k8s/base/deployment.yaml`(`SHINASADAME_MODELS` の2行を削除)
- Modify: `shinasadame/README.md`(モデル選択肢の説明を更新)

**Interfaces:**
- Consumes: Task 2 のコマンド `uv run python tools/update_models.py`

- [ ] **Step 1: フックを書く** — `shinasadame` のイメージ(`context: shinasadame`)の `hooks.before` に、`dir: shinasadame` でスクリプトを実行する。`uv` や Python が無い、またはスクリプトが失敗しても、**ビルドを止めない**(コマンドの失敗を無視する形にする。この環境の Windows で動く書き方を選ぶ)。`replay` のイメージにはフックを付けない。

- [ ] **Step 2: 環境変数を整理する** — `deployment.yaml` から `SHINASADAME_MODELS` を削除し、README の該当箇所(「選択肢にするモデルは…」)を、`models.json` と更新方法(`uv run python tools/update_models.py --force`、ビルド前に自動で回る、24 時間以内ならスキップ、コミットは手動)の説明に書き換える。`SHINASADAME_MODELS` は設定すれば優先される(上書き用)ことも書く。

- [ ] **Step 3: 検証する** — Run: `skaffold diagnose` と `skaffold render -p web` / Expected: エラーなし。次に、`uv` を PATH から外した状態で `skaffold build -p web --push=false --dry-run`(または同等のフック実行確認)で、フックが失敗しても全体が失敗扱いにならないことを確認する。可能なら通常の `skaffold run -p web` までは、利用者の確認後にする(クラスタに配備されるため)。

- [ ] **Step 4: 実機で確認する** — イメージの中に `models.json` が入り、アプリが読めることを確認する(`docker build` 後に `docker run --rm <image> python -c "from shinasadame.model_catalog import load_catalog; print(len(load_catalog()))"` が 0 より大きい)。`uv sync` が editable かどうかで、ファイルの置き場所が変わるので、読めなければ原因をここで切り分ける。

---

## Self-Review

- **Spec coverage:** 取得元と判定(Task 1)、保存形式・失敗時・件数減・鮮度(Task 2)、系列・環境変数の優先・空のとき(Task 3)、`default_model`・`model_custom`・単一の `model` フィールド(Task 4)、複数段メニュー(Task 5)、フック・`SHINASADAME_MODELS` の削除・README(Task 6)、完了の確認(Task 2 Step 5 / Task 5 Step 2 / Task 6)。漏れなし。
- **spec にない追加:** 24 時間以内なら取得をスキップする(`skaffold dev` が再ビルドのたびに 3.7MB のダウンロードを繰り返さないため)。`--force` で上書きできる。
- **spec との整合:** spec の「OpenRouter に無く LiteLLM にだけあるモデルは載せない」に対し、`-latest` の別名だけは LiteLLM から載せる(spec の「`-latest` の別名は載せる」に従う)。
- **名前の一貫性:** `select_models` / `latest_aliases` / `NAME_OVERRIDES`(Task 1)、`is_sane` / `run`(Task 2)、`Series` / `ProviderGroup` / `load_catalog` / `series_of` / `group_models`(Task 3)、`data-cascade` / `data-series`(Task 4, 5)。
