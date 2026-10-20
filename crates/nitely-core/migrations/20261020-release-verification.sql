CREATE TABLE release_verification (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    identity TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('gate', 'review')),
    gate_id INTEGER,
    passed INTEGER NOT NULL DEFAULT 0,
    evidence TEXT NOT NULL DEFAULT '',
    started_at TEXT NOT NULL,
    finished_at TEXT
);
CREATE INDEX release_verification_identity ON release_verification(identity, phase, id DESC);
