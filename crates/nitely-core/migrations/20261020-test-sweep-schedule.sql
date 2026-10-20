CREATE TABLE test_sweep_schedule (
  project TEXT PRIMARY KEY REFERENCES project(key) ON UPDATE CASCADE ON DELETE CASCADE,
  bucket TEXT NOT NULL,
  reserved_at INTEGER NOT NULL,
  issue TEXT
);
