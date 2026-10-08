import pytest

import fr_db
from shinasadame import etl, records


@pytest.fixture
def sid(engine, fr_v5):
    return etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))


def test_list_newest_first_with_candidate_counts(engine, sid):
    rows, total = records.list_records(engine, sid, "completions")
    assert total == 4
    assert [r["id"] for r in rows] == [4, 3, 2, 1]
    by_id = {r["id"]: r for r in rows}
    assert (by_id[1]["n_candidates"], by_id[1]["n_selected"]) == (3, 1)
    assert (by_id[2]["n_candidates"], by_id[2]["n_selected"]) == (3, 0)


def test_search_matches_prompt_and_candidates(engine, sid):
    rows, total = records.list_records(engine, sid, "completions", q="prompt three")
    assert [r["id"] for r in rows] == [3]
    # 候補側の語でも引ける
    rows, total = records.list_records(engine, sid, "completions", q="二番目が採用")
    assert [r["id"] for r in rows] == [3]
    assert total == 1


def test_search_escapes_like_wildcards(engine, sid):
    rows, total = records.list_records(engine, sid, "completions", q="%")
    assert (rows, total) == ([], 0)


def test_long_columns_are_truncated(engine, sid):
    rows, _ = records.list_records(engine, sid, "code_actions")
    assert rows[0]["target_text"] == "対象の文"


def test_pagination(engine, sid, monkeypatch):
    monkeypatch.setattr(records, "PAGE_SIZE", 3)
    p1, total = records.list_records(engine, sid, "completions", page=1)
    p2, _ = records.list_records(engine, sid, "completions", page=2)
    assert total == 4
    assert [r["id"] for r in p1] == [4, 3, 2]
    assert [r["id"] for r in p2] == [1]


def test_every_table_is_listable(engine, sid):
    for table in records.SPECS:
        rows, total = records.list_records(engine, sid, table)
        assert total >= 1, table
        assert rows, table


def test_detail_includes_candidates_in_rank_order(engine, sid):
    d = records.get_record(engine, sid, "completions", 1)
    assert d["row"]["prompt"] == "prompt one"
    label, table, cands = d["children"][0]
    assert table == "completion_candidates"
    assert [c["rank"] for c in cands] == [0, 1, 2]
    assert cands[0]["selected"] == 1
    assert cands[0]["confidence"] == pytest.approx(0.9)


def test_detail_character_update_sections(engine, sid):
    d = records.get_record(engine, sid, "character_updates", 1)
    _, _, sections = d["children"][0]
    assert sections[0]["character_name"] == "太郎"
    assert sections[0]["applied"] == 1


def test_detail_finding_links_to_review(engine, sid):
    rows, _ = records.list_records(engine, sid, "quality_findings")
    llm = next(r for r in rows if r["source"] == "llm")
    d = records.get_record(engine, sid, "quality_findings", llm["id"])
    assert d["review"]["id"] == 1


def test_detail_missing_returns_none(engine, sid):
    assert records.get_record(engine, sid, "completions", 999) is None
