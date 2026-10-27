-- A reviewer that could not run (not signed in, over quota, not installed): one row from the first
-- review it missed until it reviews again (NIT-362). The person is told once, not per review.
CREATE TABLE reviewer_down (
  reviewer TEXT PRIMARY KEY,
  why TEXT NOT NULL,
  hint TEXT NOT NULL DEFAULT '',
  since INTEGER NOT NULL
);
