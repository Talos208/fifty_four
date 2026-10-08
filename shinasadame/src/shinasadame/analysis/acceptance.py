"""補完の採用率。

- リクエスト単位(主指標): `selected` の候補が 1 つ以上ある completions 数 ÷ completions 総数
- 順位別: その rank で `selected` の候補数 ÷ その rank の候補総数

`selected` は挿入された文字列が候補と完全一致したときだけ立つので、手直しして採用した場合や
別の補完に上書きされた場合は未採用になる。ここで出す値はあくまで下限値。
"""

from sqlalchemy import Engine, case, exists, func, select

from ..db import FR_TABLES

C = FR_TABLES["completions"]
CAND = FR_TABLES["completion_candidates"]

BY = ("day", "model", "rank", "document")


def _filters(
    sid: int,
    date_from: str | None,
    date_to: str | None,
    model: str | None,
    document: str | None = None,
) -> list:
    conds = [C.c.snapshot_id == sid]
    day = func.substr(C.c.created_at, 1, 10)
    if date_from:
        conds.append(day >= date_from)
    if date_to:
        conds.append(day <= date_to)
    if model:
        conds.append(C.c.model_name == model)
    if document:
        conds.append(C.c.document_uri == document)
    return conds


def _accepted_expr():
    """この completion に selected な候補が 1 つ以上あれば 1。"""
    has_selected = exists().where(
        CAND.c.snapshot_id == C.c.snapshot_id,
        CAND.c.completion_id == C.c.id,
        CAND.c.selected == 1,
    )
    return case((has_selected, 1), else_=0)


def _rows(result) -> list[dict]:
    out = []
    for key, total, accepted in result:
        total = int(total or 0)
        accepted = int(accepted or 0)
        out.append(
            {
                "key": key,
                "total": total,
                "accepted": accepted,
                "rate": accepted / total if total else None,
            }
        )
    return out


def acceptance(
    engine: Engine,
    sid: int,
    by: str,
    date_from: str | None = None,
    date_to: str | None = None,
    model: str | None = None,
    document: str | None = None,
) -> list[dict]:
    if by not in BY:
        raise ValueError(f"by must be one of {BY}")
    conds = _filters(sid, date_from, date_to, model, document)

    if by == "rank":
        stmt = (
            select(
                CAND.c.rank,
                func.count(),
                func.sum(case((CAND.c.selected == 1, 1), else_=0)),
            )
            .select_from(
                CAND.join(
                    C, (CAND.c.snapshot_id == C.c.snapshot_id) & (CAND.c.completion_id == C.c.id)
                )
            )
            .where(*conds)
            .group_by(CAND.c.rank)
            .order_by(CAND.c.rank)
        )
    else:
        key = {
            "day": func.substr(C.c.created_at, 1, 10),
            "model": C.c.model_name,
            "document": C.c.document_uri,
        }[by]
        stmt = (
            select(key, func.count(), func.sum(_accepted_expr()))
            .where(*conds)
            .group_by(key)
            .order_by(key if by != "document" else func.count().desc())
        )
    with engine.connect() as conn:
        return _rows(conn.execute(stmt))


def summary(
    engine: Engine,
    sid: int,
    date_from: str | None = None,
    date_to: str | None = None,
    model: str | None = None,
    document: str | None = None,
) -> dict:
    stmt = select(func.count(), func.sum(_accepted_expr())).where(
        *_filters(sid, date_from, date_to, model, document)
    )
    with engine.connect() as conn:
        total, accepted = conn.execute(stmt).one()
    total = int(total or 0)
    accepted = int(accepted or 0)
    return {"total": total, "accepted": accepted, "rate": accepted / total if total else None}


def models(engine: Engine, sid: int) -> list[str]:
    stmt = (
        select(C.c.model_name)
        .distinct()
        .where(C.c.snapshot_id == sid, C.c.model_name.is_not(None))
        .order_by(C.c.model_name)
    )
    with engine.connect() as conn:
        return list(conn.execute(stmt).scalars())
