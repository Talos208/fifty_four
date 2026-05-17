CREATE TABLE IF NOT EXISTS character_updates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TIMESTAMP DEFAULT (datetime('now', 'subsec')),
    completed_at TIMESTAMP,
    document_uri TEXT NOT NULL,
    model_name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    response TEXT
);

CREATE TABLE IF NOT EXISTS character_update_sections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    update_id INTEGER NOT NULL,
    character_name TEXT NOT NULL,
    attribute TEXT NOT NULL,
    old_text TEXT,
    new_text TEXT NOT NULL,
    applied BOOLEAN NOT NULL,
    skip_reason TEXT,
    FOREIGN KEY (update_id) REFERENCES character_updates(id)
);
