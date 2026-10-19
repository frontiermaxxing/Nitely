CREATE TABLE project_focus (
  project TEXT PRIMARY KEY,
  goal INTEGER,
  allow_urgent INTEGER NOT NULL DEFAULT 1 CHECK (allow_urgent IN (0, 1)),
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);
