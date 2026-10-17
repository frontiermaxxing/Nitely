-- Asynchronous submit (NIT-212): one row per handed-in branch. A job runs `submit` in a process
-- of its own; its result is today's Submitted, as JSON.
CREATE TABLE IF NOT EXISTS submit_job (
  id           INTEGER PRIMARY KEY,
  issue_key    TEXT NOT NULL,
  worktree     TEXT NOT NULL,
  branch       TEXT NOT NULL,
  head         TEXT NOT NULL,
  requested_by TEXT NOT NULL,
  skip_review  INTEGER NOT NULL DEFAULT 0,
  local_only   INTEGER NOT NULL DEFAULT 0,
  state        TEXT NOT NULL,              -- queued, running, done, failed, superseded
  queued_at    INTEGER NOT NULL,
  started_at   INTEGER,
  ended_at     INTEGER,
  pid          INTEGER,
  result       TEXT,
  error        TEXT,
  delivered_at INTEGER
);
CREATE INDEX IF NOT EXISTS submit_job_issue ON submit_job (issue_key COLLATE NOCASE, id);
CREATE INDEX IF NOT EXISTS submit_job_requester ON submit_job (requested_by, delivered_at);
