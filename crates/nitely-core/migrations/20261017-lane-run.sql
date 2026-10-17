-- Build lane requests and what came of them (lanes.rs, docs/build-lanes.md, NIT-197).
CREATE TABLE lane_run (
  id             INTEGER PRIMARY KEY,
  project        TEXT NOT NULL,
  repo           TEXT NOT NULL,            -- the repo's main checkout
  issue_key      TEXT,
  requested_by   TEXT NOT NULL,            -- the actor id
  kind           TEXT NOT NULL,            -- run | gate | land
  snapshot       TEXT NOT NULL,            -- the commit built
  tree           TEXT NOT NULL,
  base           TEXT NOT NULL,            -- the commit the work sits on
  command        TEXT NOT NULL,
  state          TEXT NOT NULL,            -- queued | running | done | lane_error | abandoned
  priority       INTEGER NOT NULL DEFAULT 3,
  lane           INTEGER,
  executor       TEXT NOT NULL DEFAULT 'local',
  pid            INTEGER,                  -- the process serving it, so a crash reads as abandoned
  queued_at      INTEGER NOT NULL,
  started_at     INTEGER,
  ended_at       INTEGER,
  exit_code      INTEGER,
  timed_out      INTEGER NOT NULL DEFAULT 0,
  setup_ran      INTEGER NOT NULL DEFAULT 0,
  files_switched INTEGER,
  log_path       TEXT,
  output         TEXT NOT NULL DEFAULT '',
  error          TEXT
);
CREATE INDEX lane_run_project ON lane_run (project, id);
