-- Triage (NIT-245): agents' questions about the board or the code, answered by a triage agent
-- instead of the person; only one that turns out to be the person's becomes an ask.
CREATE TABLE IF NOT EXISTS triage (
  id           INTEGER PRIMARY KEY,
  issue_key    TEXT,
  question     TEXT NOT NULL,
  asked_by     TEXT NOT NULL,
  place        TEXT,                -- where the agent asked from, to find the repository
  state        TEXT NOT NULL,       -- open, answered, escalated, failed
  answer       TEXT,
  ask_id       INTEGER,             -- escalated: the person's ask it became
  error        TEXT,
  asked_at     INTEGER NOT NULL,
  answered_at  INTEGER,
  pid          INTEGER,             -- the process answering it
  delivered_at INTEGER              -- when the asking agent heard the answer
);
CREATE INDEX IF NOT EXISTS triage_asker ON triage (asked_by, delivered_at);
CREATE INDEX IF NOT EXISTS triage_issue ON triage (issue_key COLLATE NOCASE, id);
