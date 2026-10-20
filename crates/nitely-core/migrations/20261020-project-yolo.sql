CREATE TABLE project_yolo (
  project TEXT PRIMARY KEY,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
  revision INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);
