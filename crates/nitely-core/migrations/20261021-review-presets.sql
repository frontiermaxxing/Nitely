-- Fast, Careful and Release replace YOLO, Fast, Balanced and Thorough (NIT-491). Each project moves
-- to the closest preset and keeps what it moved from, so the app can show what changed until the
-- person saves a mode. Custom keeps its overrides.
CREATE TABLE project_review_mode_next (
 project TEXT PRIMARY KEY REFERENCES project(key),
 mode TEXT NOT NULL CHECK(mode IN ('fast','careful','release','custom')),
 overrides TEXT NOT NULL DEFAULT '{}',
 revision INTEGER NOT NULL DEFAULT 0,
 explicit INTEGER NOT NULL DEFAULT 0,
 updated_at INTEGER NOT NULL DEFAULT 0,
 updated_by TEXT NOT NULL DEFAULT 'migration',
 migrated_from TEXT
);
INSERT INTO project_review_mode_next(project,mode,overrides,revision,explicit,updated_at,updated_by,migrated_from)
 SELECT project,CASE mode WHEN 'yolo' THEN 'fast' WHEN 'balanced' THEN 'careful' WHEN 'thorough' THEN 'release' ELSE mode END,
  overrides,revision,explicit,updated_at,updated_by,CASE WHEN mode='custom' THEN NULL ELSE mode END
 FROM project_review_mode;
DROP TABLE project_review_mode;
ALTER TABLE project_review_mode_next RENAME TO project_review_mode;
-- Every policy a project has had, by its authority, so work queued under one can be compared with
-- the current one: a looser mode lets it finish (NIT-487).
CREATE TABLE project_review_mode_authority (
 authority TEXT PRIMARY KEY,
 project TEXT NOT NULL,
 options TEXT NOT NULL,
 recorded_at INTEGER NOT NULL
);
