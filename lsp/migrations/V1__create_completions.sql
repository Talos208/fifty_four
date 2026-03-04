CREATE TABLE IF NOT EXISTS completions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TIMESTAMP DEFAULT (datetime('now', 'subsec')),
    document_uri TEXT NOT NULL,
    cursor_line INTEGER NOT NULL,
    cursor_character INTEGER NOT NULL,
    model_name TEXT NOT NULL,
    prompt TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS completion_candidates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    completion_id INTEGER NOT NULL,
    rank INTEGER NOT NULL,
    candidate TEXT NOT NULL,
    selected BOOLEAN DEFAULT false,
    FOREIGN KEY (completion_id) REFERENCES completions(id)
);
