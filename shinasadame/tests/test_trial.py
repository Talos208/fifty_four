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

    async def run(self, model, prompt, timeout_secs=None):
        self.calls.append((model, prompt))
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


def test_original_output_uses_response_or_joined_candidates(engine, sid):
    ca = records.get_record(engine, sid, "code_actions", 1)
    assert records.original_output(ca) == '{"candidates":[]}'
    comp = records.get_record(engine, sid, "completions", 1)
    assert records.original_output(comp) == "採用された続き\n---\n別案その一\n---\n別案その二"
    finding = records.get_record(engine, sid, "quality_findings", 1)
    assert records.original_output(finding) is None


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


def test_try_shows_original_and_trial_side_by_side(client, sid):
    fake = use(FakeReplay(result=OK))
    res = client.post(
        f"/s/{sid}/records/code_actions/1/try",
        data={"prompt": "編集後のプロンプト", "model": "google/gemini-3.1-pro"},
    )

    assert res.status_code == 200
    assert fake.calls == [("google/gemini-3.1-pro", "編集後のプロンプト")]
    assert "試行の応答" in res.text
    assert '{&#34;candidates&#34;:[]}' in res.text or '{"candidates":[]}' in res.text  # 元の応答
    assert "1234" in res.text  # elapsed_ms
    assert "google/gemini-3.1-pro" in res.text


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
    assert '<textarea name="prompt" rows="12" required>\n\n本文</textarea>' in page


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
