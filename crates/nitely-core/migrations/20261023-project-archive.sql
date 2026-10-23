ALTER TABLE project ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));

-- Match case-insensitive history joins without scanning every issue per event.
CREATE INDEX issue_archive_key ON issue (key COLLATE NOCASE, project);
CREATE INDEX project_archive_key ON project (key COLLATE NOCASE, archived);

-- Default cross-project reads share this filter; explicit issue/history reads keep their data.
CREATE VIEW active_issue AS
SELECT i.* FROM issue i WHERE NOT EXISTS
  (SELECT 1 FROM project p WHERE p.key = i.project COLLATE NOCASE AND p.archived = 1);
CREATE VIEW active_event AS
SELECT e.* FROM event e WHERE NOT EXISTS
  (SELECT 1 FROM issue i JOIN project p ON p.key = i.project COLLATE NOCASE
   WHERE i.key = e.issue_key COLLATE NOCASE AND p.archived = 1)
AND NOT EXISTS (SELECT 1 FROM project p
  WHERE p.key = json_extract(CASE WHEN json_valid(e.payload) THEN e.payload ELSE '{}' END, '$.project') COLLATE NOCASE AND p.archived = 1);
CREATE VIEW active_ask AS
SELECT a.* FROM ask a WHERE NOT EXISTS
  (SELECT 1 FROM issue i JOIN project p ON p.key = i.project COLLATE NOCASE
   WHERE i.key = a.issue_key COLLATE NOCASE AND p.archived = 1);
