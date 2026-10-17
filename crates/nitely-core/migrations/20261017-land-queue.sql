-- The land queue (land_queue.rs, NIT-215, docs/batching.md): with `[land] batch`, changes that wait
-- while a land runs land together. One row per change handed to the queue, and one per queue run.
CREATE TABLE land_queue (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    issue_key TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    push INTEGER NOT NULL DEFAULT 1,
    keep INTEGER NOT NULL DEFAULT 0,
    -- queued | running | landed | sent_back
    state TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    ended_at INTEGER,
    run_id INTEGER,
    -- What land returned for it, as JSON.
    result TEXT,
    delivered_at INTEGER
);
CREATE INDEX land_queue_repo_state ON land_queue (repo, state);
CREATE TABLE land_run (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    -- The run's changes in stack order, then what became of them; keys, comma-separated.
    issues TEXT NOT NULL,
    landed TEXT NOT NULL DEFAULT '',
    sent_back TEXT NOT NULL DEFAULT '',
    gates INTEGER NOT NULL DEFAULT 0,
    -- The integration branch's head after the run, short.
    head TEXT
);
