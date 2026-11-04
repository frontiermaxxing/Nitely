-- Tracking (NIT-739): a project that uses Nitely to coordinate agents, with no checks on what they
-- land. A mode of its own, so the CHECK takes one more name; every row is copied as it is.
CREATE TABLE project_review_mode_next (
 project TEXT PRIMARY KEY REFERENCES project(key),
 mode TEXT NOT NULL CHECK(mode IN ('fast','careful','release','custom','tracking')),
 overrides TEXT NOT NULL DEFAULT '{}',
 revision INTEGER NOT NULL DEFAULT 0,
 explicit INTEGER NOT NULL DEFAULT 0,
 updated_at INTEGER NOT NULL DEFAULT 0,
 updated_by TEXT NOT NULL DEFAULT 'migration',
 migrated_from TEXT
);
INSERT INTO project_review_mode_next(project,mode,overrides,revision,explicit,updated_at,updated_by,migrated_from)
 SELECT project,mode,overrides,revision,explicit,updated_at,updated_by,migrated_from FROM project_review_mode;
DROP TABLE project_review_mode;
ALTER TABLE project_review_mode_next RENAME TO project_review_mode;
