-- Quick changes waiting for, and given, their review after landing (NIT-625): one row per
-- issue, its landed range, and once reviewed when, by whom and how it came out (passed, findings,
-- or gone when the range left the branch's history).
CREATE TABLE quick_review (
    issue_key TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    repo TEXT NOT NULL,
    base TEXT NOT NULL,
    head TEXT NOT NULL,
    landed_at INTEGER NOT NULL,
    reviewed_at INTEGER,
    reviewer TEXT,
    outcome TEXT
);
