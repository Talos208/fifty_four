import re
import pytest
from fastapi.testclient import TestClient

from shinasadame import db, etl, records
from shinasadame.app import app, get_replay_client
from shinasadame.replay import ReplayFailed, ReplayOk, ReplayUnavailable

import fr_db


class FakeReplay:
    def __init__(self, result=None, error=None):
        self.result = result
        self.error = error
        self.calls = []
        self.options_from = []

    async def run(self, model, prompt, timeout_secs=None, options_from=None):
        self.calls.append((model, prompt))
        self.options_from.append(options_from)
        if self.error:
            raise self.error
        return self.result


@pytest.fixture
def client():
    with TestClient(app) as c:
        yield c
    app.dependency_overrides.clear()


@pytest.fixture
def sid(engine, fr_v5):
    return etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))


def use(fake):
    app.dependency_overrides[get_replay_client] = lambda: fake
    return fake


OK = ReplayOk(response="試行の応答", model="google/gemini-3.1-pro", elapsed_ms=1234, truncated=False)


def test_prompt_tables_are_derived_from_columns():
    assert records.PROMPT_TABLES == {"completions", "code_actions", "character_updates", "quality_reviews"}


def test_original_output_is_gone():
    assert not hasattr(records, "original_output")


def test_structured_templates_are_defined_for_completions_only():
    assert records.TRIAL_TEMPLATES == {"completions": "prompt_completion.md"}
    assert set(records.TRIAL_TEMPLATES) <= records.PROMPT_TABLES


def test_detail_page_shows_try_panel_with_original_prompt_and_note(client, sid, monkeypatch):
    monkeypatch.delenv("REPLAY_LMSTUDIO_HOST", raising=False)
    monkeypatch.setenv("SHINASADAME_MODELS", "google/gemini-3.1-pro,anthropic/claude-sonnet-5-5")
    page = client.get(f"/s/{sid}/records/completions/1").text

    assert f'hx-post="/s/{sid}/records/completions/1/try"' in page
    assert "prompt one" in page  # 元のプロンプトが編集欄の初期値
    # 記録の model_name を replay が解釈できる形にしたものが既定値(素の名前は LM Studio 扱い)
    # モデルは本物のドロップダウン(<select>)。datalist だと入力済みの文字に合う候補しか出ない
    assert '<select name="model"' in page
    assert "<datalist" not in page
    # 記録の model_name を replay が解釈できる形にしたものが先頭で選択済み(素の名前は LM Studio 扱い)
    assert (
        '<option value="lmstudio@host.docker.internal:1234/model-a" selected>' in page
    )
    # SHINASADAME_MODELS の候補が全部、選択肢に並ぶ
    assert '<option value="google/gemini-3.1-pro"' in page
    assert '<option value="anthropic/claude-sonnet-5-5"' in page
    # 一覧にないモデルは自由入力欄で指定できる
    assert 'name="model_custom"' in page
    assert "tool なしで実行します" in page


def test_model_options_put_default_first_without_duplicates(client, sid, monkeypatch):
    monkeypatch.delenv("REPLAY_LMSTUDIO_HOST", raising=False)
    # 既定値と同じものが候補にもあっても、選択肢は重複しない
    monkeypatch.setenv(
        "SHINASADAME_MODELS",
        "lmstudio@host.docker.internal:1234/model-a,google/gemini-3.1-pro",
    )
    page = client.get(f"/s/{sid}/records/completions/1").text
    assert page.count('<option value="lmstudio@host.docker.internal:1234/model-a"') == 1


def test_try_prefers_custom_model_over_selected(client, sid):
    fake = use(FakeReplay(result=OK))
    client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "p", "model": "google/gemini-3.1-pro", "model_custom": "  xai/grok-4.7  "},
    )
    assert fake.calls == [("xai/grok-4.7", "p")]


def test_try_uses_selected_model_when_custom_is_blank(client, sid):
    fake = use(FakeReplay(result=OK))
    client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "p", "model": "google/gemini-3.1-pro", "model_custom": "   "},
    )
    assert fake.calls == [("google/gemini-3.1-pro", "p")]


def test_detail_page_hides_try_panel_for_tables_without_prompt(client, sid):
    page = client.get(f"/s/{sid}/records/quality_findings/1").text
    assert "/try" not in page


def test_try_shows_the_trial_result_without_the_original_output(client, sid):
    fake = use(FakeReplay(result=OK))
    res = client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "編集後のプロンプト", "model": "google/gemini-3.1-pro"},
    )

    assert res.status_code == 200
    assert fake.calls == [("google/gemini-3.1-pro", "編集後のプロンプト")]
    assert "試行の応答" in res.text
    assert "元の出力" not in res.text  # 元の出力は候補の表で見られる
    assert "1234" in res.text  # elapsed_ms
    assert "google/gemini-3.1-pro" in res.text
    assert fake.options_from == [None]  # code_actions は構造化出力のテンプレートを指定しない


STRUCTURED = (
    '{"candidates":[{"text":"一つ目の候補","confidence":0.8},'
    '{"text":"二つ目の候補","confidence":0.35}]}'
)


def test_try_completions_asks_for_structured_output_and_shows_confidence(client, sid):
    fake = use(FakeReplay(result=ReplayOk(STRUCTURED, "google/x", 10, False)))
    res = client.post(
        f"/s/{sid}/records/completions/1/try", data={"prompt": "p", "model": "google/x"}
    )

    assert fake.options_from == ["prompt_completion.md"]
    # 記録の候補と同じ見せ方(順位・候補・confidence の表)
    assert "一つ目の候補" in res.text and "二つ目の候補" in res.text
    assert "0.80" in res.text and "0.35" in res.text
    assert "<table" in res.text
    assert "confidence" in res.text


def test_try_completions_escapes_candidate_text(client, sid):
    body = '{"candidates":[{"text":"<script>alert(1)</script>","confidence":0.5}]}'
    use(FakeReplay(result=ReplayOk(body, "m", 1, False)))
    res = client.post(f"/s/{sid}/records/completions/1/try", data={"prompt": "p", "model": "google/x"})
    assert "<script>alert(1)</script>" not in res.text
    assert "&lt;script&gt;" in res.text


@pytest.mark.parametrize(
    "body",
    [
        "JSON ではない自由文",
        '{"candidates": "not a list"}',
        '{"other": 1}',
        '["candidates"]',
        '{"candidates":[{"text":"x"}]}',  # confidence が無くても表示は壊さない
    ],
)
def test_try_completions_falls_back_to_raw_text_when_not_structured(client, sid, body):
    use(FakeReplay(result=ReplayOk(body, "m", 1, False)))
    res = client.post(f"/s/{sid}/records/completions/1/try", data={"prompt": "p", "model": "google/x"})
    assert res.status_code == 200
    assert "Traceback" not in res.text


def test_try_escapes_html_in_llm_output(client, sid):
    use(FakeReplay(result=ReplayOk("<script>alert(1)</script>", "m", 1, False)))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert "<script>alert(1)</script>" not in res.text
    assert "&lt;script&gt;" in res.text


def test_try_notes_truncation(client, sid):
    use(FakeReplay(result=ReplayOk("長い応答", "m", 1, True)))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert "切り詰め" in res.text


@pytest.mark.parametrize(
    ("kind", "needle"),
    [
        ("bad_request", "リクエストが不正です"),
        ("auth", "認証に失敗しました"),
        ("timeout", "タイムアウト"),
        ("llm", "LLM がエラーを返しました"),
        ("internal", "replay サービスの内部エラー"),
    ],
)
def test_try_shows_error_kinds_with_http_200(client, sid, kind, needle):
    use(FakeReplay(result=ReplayFailed(kind=kind, message="詳細メッセージ")))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert res.status_code == 200  # htmx がスワップできるよう 200
    assert needle in res.text
    assert "詳細メッセージ" in res.text


def test_try_when_replay_is_down_does_not_500(client, sid):
    use(FakeReplay(error=ReplayUnavailable("refused")))
    res = client.post(f"/s/{sid}/records/code_actions/1/try", data={"prompt": "p", "model": "google/x"})
    assert res.status_code == 200
    assert "replay サービスに接続できません" in res.text


def test_try_unknown_table_record_and_snapshot_are_404(client, sid):
    use(FakeReplay(result=OK))
    form = {"prompt": "p", "model": "google/x"}
    assert client.post(f"/s/{sid}/records/quality_findings/1/try", data=form).status_code == 404
    assert client.post(f"/s/{sid}/records/nosuch/1/try", data=form).status_code == 404
    assert client.post(f"/s/{sid}/records/completions/999/try", data=form).status_code == 404
    assert client.post("/s/999/records/completions/1/try", data=form).status_code == 404


def test_try_requires_prompt_and_model(client, sid):
    use(FakeReplay(result=OK))
    assert client.post(f"/s/{sid}/records/completions/1/try", data={"model": "google/x"}).status_code == 422
    assert client.post(f"/s/{sid}/records/completions/1/try", data={"prompt": "p"}).status_code == 422


def test_textarea_has_leading_newline_so_a_leading_newline_in_prompt_survives(client, sid):
    # HTML パーサは <textarea> 直後の改行を 1 つ捨てる。先頭が改行のプロンプトを守るため 1 つ足す。
    with db.get_engine().begin() as conn:
        t = db.FR_TABLES["completions"]
        conn.execute(t.update().where(t.c.snapshot_id == sid, t.c.id == 1).values(prompt="\n本文"))
    page = client.get(f"/s/{sid}/records/completions/1").text
    assert re.search(r'<textarea name="prompt" rows="\d+" required>\n\n本文</textarea>', page)


def test_try_normalises_crlf_in_prompt(client, sid):
    fake = use(FakeReplay(result=OK))
    client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "一行目\r\n二行目", "model": "google/x"},
    )
    assert fake.calls == [("google/x", "一行目\n二行目")]


def test_edit_area_escapes_closing_textarea(client, sid):
    with db.get_engine().begin() as conn:
        conn.execute(
            db.FR_TABLES["completions"]
            .update()
            .where(db.FR_TABLES["completions"].c.snapshot_id == sid, db.FR_TABLES["completions"].c.id == 1)
            .values(prompt="前</textarea><b>後")
        )
    page = client.get(f"/s/{sid}/records/completions/1").text
    assert "</textarea><b>" not in page
    assert "&lt;/textarea&gt;" in page


# ---- 画面のレイアウト: パラメータ → 候補 → 試す ----


def test_detail_page_orders_params_then_candidates_then_try(client, sid):
    page = client.get(f"/s/{sid}/records/completions/1").text
    params = page.index('<dl class="fields">')
    candidates = page.index("候補 <small")
    try_panel = page.index('class="trial"')
    assert params < candidates < try_panel


def test_prompt_and_model_inputs_are_hidden_until_expanded(client, sid):
    page = client.get(f"/s/{sid}/records/completions/1").text
    # 編集欄(プロンプト・モデル)は閉じた <details> の中。「試す」ボタンは外に出ていて、すぐ押せる
    start = page.index('<details class="trial-edit"')
    end = page.index("</details>", start)
    inner = page[start:end]
    assert " open" not in page[start : page.index(">", start)]
    assert 'name="prompt"' in inner and 'name="model"' in inner and 'name="model_custom"' in inner
    assert '<button type="submit">' in page[end:]


def test_prompt_field_uses_the_same_grid_as_the_parameters(client, sid):
    page = client.get(f"/s/{sid}/records/completions/1").text
    # パラメータの欄(dl.fields)と同じ列幅・フォントで揃える
    assert 'class="fields trial-fields"' in page
    assert "<dt>プロンプト</dt>" in page


def test_textarea_height_follows_the_prompt_length(client, sid):
    # 中身より低いと内側にスクロールが出て、パラメータの欄(全文が見える)と見た目が揃わない。
    # field-sizing 非対応のブラウザ向けに、行数から rows も出す。
    body = "\n".join(f"{i} 行目" for i in range(30))
    with db.get_engine().begin() as conn:
        t = db.FR_TABLES["completions"]
        conn.execute(t.update().where(t.c.snapshot_id == sid, t.c.id == 1).values(prompt=body))
    page = client.get(f"/s/{sid}/records/completions/1").text
    assert 'rows="30"' in page


def test_textarea_has_a_minimum_rows_for_short_prompts(client, sid):
    page = client.get(f"/s/{sid}/records/completions/1").text  # "prompt one"(1 行)
    assert 'rows="6"' in page


def test_textarea_css_grows_with_content():
    from pathlib import Path

    css = (Path(__file__).parent.parent / "src/shinasadame/static/style.css").read_text(encoding="utf-8")
    block = css[css.index(".trial-fields textarea") :].split("}")[0]
    assert "field-sizing: content" in block
    assert "min-height: 12em" not in block  # 固定の最小高さで中身を切らない
