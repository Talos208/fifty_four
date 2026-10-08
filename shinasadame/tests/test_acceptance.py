import pytest

import fr_db
from shinasadame import etl
from shinasadame.analysis import acceptance as acc


@pytest.fixture
def sid(engine, fr_v5):
    return etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))


def by_key(rows):
    return {r["key"]: (r["total"], r["accepted"]) for r in rows}


def test_summary_is_request_level(engine, sid):
    s = acc.summary(engine, sid)
    assert (s["total"], s["accepted"]) == (4, 2)
    assert s["rate"] == pytest.approx(0.5)


def test_by_rank_counts_candidates_not_requests(engine, sid):
    rows = acc.acceptance(engine, sid, "rank")
    assert by_key(rows) == {0: (4, 1), 1: (3, 1), 2: (2, 0)}
    assert [r["key"] for r in rows] == [0, 1, 2]
    assert rows[1]["rate"] == pytest.approx(1 / 3)


def test_by_day(engine, sid):
    assert by_key(acc.acceptance(engine, sid, "day")) == {
        "2026-10-01": (2, 1),
        "2026-10-02": (1, 1),
        "2026-10-03": (1, 0),
    }


def test_by_model_and_document(engine, sid):
    assert by_key(acc.acceptance(engine, sid, "model")) == {"model-a": (2, 1), "model-b": (2, 1)}
    assert by_key(acc.acceptance(engine, sid, "document")) == {
        "file:///a.txt": (2, 1),
        "file:///b.txt": (2, 1),
    }


def test_filters_apply_to_every_view(engine, sid):
    s = acc.summary(engine, sid, model="model-b")
    assert (s["total"], s["accepted"]) == (2, 1)
    assert by_key(acc.acceptance(engine, sid, "rank", model="model-b")) == {0: (2, 0), 1: (1, 1)}

    s = acc.summary(engine, sid, date_from="2026-10-02", date_to="2026-10-02")
    assert (s["total"], s["accepted"]) == (1, 1)
    assert by_key(acc.acceptance(engine, sid, "day", date_from="2026-10-02")) == {
        "2026-10-02": (1, 1),
        "2026-10-03": (1, 0),
    }


def test_document_filter(engine, sid):
    doc_b = "file:///b.txt"
    s = acc.summary(engine, sid, document=doc_b)
    assert (s["total"], s["accepted"]) == (2, 1)
    # 順位別も文書で絞れる(b.txt は c3 の 2 番目の候補だけが採用)
    assert by_key(acc.acceptance(engine, sid, "rank", document=doc_b)) == {0: (2, 0), 1: (1, 1)}
    assert by_key(acc.acceptance(engine, sid, "day", document=doc_b)) == {
        "2026-10-02": (1, 1),
        "2026-10-03": (1, 0),
    }
    # 他の条件とも組み合わせられる
    s = acc.summary(engine, sid, document=doc_b, date_from="2026-10-03")
    assert (s["total"], s["accepted"]) == (1, 0)


def test_empty_result_has_no_rate(engine, sid):
    s = acc.summary(engine, sid, model="no-such-model")
    assert s == {"total": 0, "accepted": 0, "rate": None}


def test_snapshots_do_not_mix(engine, fr_v5, sid):
    second = etl.import_snapshot(engine, fr_v5, fr_db.info(fr_v5))
    assert acc.summary(engine, second)["total"] == 4
    assert acc.summary(engine, sid)["total"] == 4


def test_unknown_by_is_rejected(engine, sid):
    with pytest.raises(ValueError):
        acc.acceptance(engine, sid, "nope")


def test_models_list(engine, sid):
    assert acc.models(engine, sid) == ["model-a", "model-b"]
