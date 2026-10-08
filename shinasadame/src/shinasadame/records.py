"""全テーブルのレコード一覧・検索・詳細。"""

from dataclasses import dataclass, field

from sqlalchemy import Engine, Table, and_, exists, func, or_, select

from .db import FR_TABLES

PAGE_SIZE = 50
SNIPPET = 100


@dataclass(frozen=True)
class Child:
    """詳細画面に並べる子テーブル。"""

    label: str
    table: str
    fk: str
    order_by: str = "id"


@dataclass(frozen=True)
class Spec:
    label: str
    # 一覧に出す列。長文列は先頭 SNIPPET 文字に切り詰める。
    columns: list[str]
    long_columns: list[str] = field(default_factory=list)
    # LIKE 検索の対象列
    search: list[str] = field(default_factory=list)
    # 子テーブルの列も検索対象にする場合 (子テーブル, 親参照列, 子の列)
    search_child: tuple[str, str, str] | None = None
    # 一覧に「候補数 / 採用数」を出す子テーブル (子テーブル, 親参照列)
    count_child: tuple[str, str] | None = None
    children: list[Child] = field(default_factory=list)


SPECS: dict[str, Spec] = {
    "completions": Spec(
        "補完",
        columns=["id", "created_at", "document_uri", "model_name"],
        long_columns=["prompt"],
        search=["prompt"],
        search_child=("completion_candidates", "completion_id", "candidate"),
        count_child=("completion_candidates", "completion_id"),
        children=[Child("候補", "completion_candidates", "completion_id", "rank")],
    ),
    "code_actions": Spec(
        "コードアクション",
        columns=["id", "created_at", "document_uri", "mode", "model_name"],
        long_columns=["target_text"],
        search=["prompt", "target_text"],
        search_child=("code_action_candidates", "code_action_id", "candidate"),
        count_child=("code_action_candidates", "code_action_id"),
        children=[Child("候補", "code_action_candidates", "code_action_id", "rank")],
    ),
    "character_updates": Spec(
        "キャラクター更新",
        columns=["id", "started_at", "completed_at", "document_uri", "model_name"],
        long_columns=["prompt"],
        search=["prompt", "response"],
        children=[Child("更新項目", "character_update_sections", "update_id")],
    ),
    "quality_reviews": Spec(
        "品質レビュー(LLM)",
        columns=["id", "created_at", "document_uri", "model_name"],
        long_columns=["prompt"],
        search=["prompt", "response"],
        children=[Child("指摘", "quality_findings", "review_id")],
    ),
    "quality_findings": Spec(
        "品質指摘",
        columns=["id", "created_at", "document_uri", "source", "rule_code", "severity", "line"],
        long_columns=["excerpt", "message"],
        search=["excerpt", "message"],
    ),
}


def _like(term: str) -> str:
    escaped = term.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
    return f"%{escaped}%"


def _search_cond(spec: Spec, t: Table, sid: int, q: str):
    pattern = _like(q)
    conds = [t.c[c].like(pattern, escape="\\") for c in spec.search]
    if spec.search_child:
        child_name, fk, col = spec.search_child
        child = FR_TABLES[child_name]
        conds.append(
            exists().where(
                child.c.snapshot_id == sid,
                child.c[fk] == t.c.id,
                child.c[col].like(pattern, escape="\\"),
            )
        )
    return or_(*conds)


def list_records(
    engine: Engine, sid: int, table: str, q: str = "", page: int = 1
) -> tuple[list[dict], int]:
    """(行, 総件数)。新しい id 順。"""
    spec = SPECS[table]
    t = FR_TABLES[table]
    where = [t.c.snapshot_id == sid]
    if q:
        where.append(_search_cond(spec, t, sid, q))

    cols = [t.c[c] for c in spec.columns]
    cols += [func.substr(t.c[c], 1, SNIPPET).label(c) for c in spec.long_columns]
    if spec.count_child:
        child_name, fk = spec.count_child
        child = FR_TABLES[child_name]
        link = and_(child.c.snapshot_id == sid, child.c[fk] == t.c.id)
        cols.append(select(func.count()).where(link).scalar_subquery().label("n_candidates"))
        cols.append(
            select(func.count()).where(link, child.c.selected == 1).scalar_subquery().label("n_selected")
        )

    stmt = (
        select(*cols)
        .where(*where)
        .order_by(t.c.id.desc())
        .limit(PAGE_SIZE)
        .offset((max(page, 1) - 1) * PAGE_SIZE)
    )
    with engine.connect() as conn:
        rows = [dict(r._mapping) for r in conn.execute(stmt)]
        total = conn.execute(select(func.count()).select_from(t).where(*where)).scalar_one()
    return rows, total


def get_record(engine: Engine, sid: int, table: str, record_id: int) -> dict | None:
    """{"row": {...}, "children": [(ラベル, 子のテーブル名, [行...])], "review": 関連レビュー or None}"""
    spec = SPECS[table]
    t = FR_TABLES[table]
    with engine.connect() as conn:
        row = conn.execute(
            select(t).where(t.c.snapshot_id == sid, t.c.id == record_id)
        ).first()
        if row is None:
            return None
        data = dict(row._mapping)
        children = []
        for ch in spec.children:
            ct = FR_TABLES[ch.table]
            rows = conn.execute(
                select(ct)
                .where(ct.c.snapshot_id == sid, ct.c[ch.fk] == record_id)
                .order_by(ct.c[ch.order_by])
            )
            children.append((ch.label, ch.table, [dict(r._mapping) for r in rows]))
        review = None
        if table == "quality_findings" and data.get("review_id") is not None:
            rt = FR_TABLES["quality_reviews"]
            r = conn.execute(
                select(rt).where(rt.c.snapshot_id == sid, rt.c.id == data["review_id"])
            ).first()
            review = dict(r._mapping) if r else None
    return {"row": data, "children": children, "review": review}
