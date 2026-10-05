-- 文章品質診断(quality)の記録。決定的ルールと LLM 診断の両方の指摘を残し、
-- 後から誤検知の傾向・LLM の抜粋捏造率・ルールの効き具合を検証できるようにする。

CREATE TABLE IF NOT EXISTS quality_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TIMESTAMP DEFAULT (datetime('now', 'subsec')),
    document_uri TEXT NOT NULL,
    model_name TEXT NOT NULL,
    prompt TEXT NOT NULL,          -- LLM に送った 1 batch 分のプロンプト
    response TEXT                  -- LLM の生応答(パース前)
);

CREATE TABLE IF NOT EXISTS quality_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TIMESTAMP DEFAULT (datetime('now', 'subsec')),
    document_uri TEXT NOT NULL,
    source TEXT NOT NULL,          -- 'rule' | 'llm'
    rule_code TEXT NOT NULL,       -- RuleId::code (LLM は 'llm-review')
    severity TEXT NOT NULL,        -- 'warning' | 'information' | 'hint'
    line INTEGER NOT NULL,         -- 0 始まりの行番号(記録時点)
    excerpt TEXT NOT NULL,         -- 指摘範囲の本文(長い場合は切り詰め)
    message TEXT NOT NULL,
    review_id INTEGER,             -- source='llm' のときの quality_reviews.id
    FOREIGN KEY (review_id) REFERENCES quality_reviews(id)
);
