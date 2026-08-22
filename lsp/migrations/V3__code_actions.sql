CREATE TABLE IF NOT EXISTS code_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TIMESTAMP DEFAULT (datetime('now', 'subsec')),
    document_uri TEXT NOT NULL,
    mode TEXT NOT NULL,            -- 'fill_mark' | 'rephrase'
    target_text TEXT NOT NULL,     -- 選択範囲のテキスト({{TARGET}} に入る値)
    model_name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    response TEXT                  -- LLM の生応答(パース前)
);

CREATE TABLE IF NOT EXISTS code_action_candidates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code_action_id INTEGER NOT NULL,
    rank INTEGER NOT NULL,
    candidate TEXT NOT NULL,
    selected BOOLEAN DEFAULT false,
    FOREIGN KEY (code_action_id) REFERENCES code_actions(id)
);
