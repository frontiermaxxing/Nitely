-- Batched review (NIT-216): changes that wait to be reviewed together, the batches they were
-- reviewed in, and each review row's batch.
CREATE TABLE IF NOT EXISTS review_queue (
  id         INTEGER PRIMARY KEY,
  issue_key  TEXT NOT NULL,
  repo       TEXT NOT NULL,              -- the repository's root: a batch holds one repository's changes
  reviewer   TEXT NOT NULL,              -- a batch has one reviewer
  note       TEXT NOT NULL DEFAULT '',   -- the reviewer choice's note, as Review::note
  title      TEXT NOT NULL,
  branch     TEXT NOT NULL,
  worktree   TEXT NOT NULL,
  base       TEXT NOT NULL,              -- the commits its diff runs between
  head       TEXT NOT NULL,
  lines      INTEGER NOT NULL,
  files      TEXT NOT NULL DEFAULT '[]',
  pid        INTEGER NOT NULL,           -- the submit's process, which waits for the result
  actor      TEXT NOT NULL,              -- who submitted it, for its timeline
  state      TEXT NOT NULL,              -- waiting, reviewing, done, alone, superseded, abandoned
  batch_id   INTEGER,
  queued_at  INTEGER NOT NULL,
  result     TEXT,                       -- done: the change's Review, as JSON
  why        TEXT                        -- alone: why it is reviewed by itself after all
);
CREATE INDEX IF NOT EXISTS review_queue_waiting ON review_queue (repo, reviewer, state, id);
CREATE INDEX IF NOT EXISTS review_queue_issue ON review_queue (issue_key COLLATE NOCASE, id);
CREATE TABLE IF NOT EXISTS review_batch (
  id          INTEGER PRIMARY KEY,
  repo        TEXT NOT NULL,
  reviewer    TEXT NOT NULL,
  issues      TEXT NOT NULL DEFAULT '[]', -- the changes' issue keys, in the prompt's order
  state       TEXT NOT NULL,              -- reviewing, done, failed
  pid         INTEGER NOT NULL,           -- the process running the reviewer
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  seconds     INTEGER,
  error       TEXT
);
ALTER TABLE review ADD COLUMN batch_id INTEGER;
