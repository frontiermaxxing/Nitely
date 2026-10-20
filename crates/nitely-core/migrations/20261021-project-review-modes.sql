CREATE TABLE project_review_mode (
 project TEXT PRIMARY KEY REFERENCES project(key),
 mode TEXT NOT NULL CHECK(mode IN ('yolo','fast','balanced','thorough','custom')),
 overrides TEXT NOT NULL DEFAULT '{}',
 revision INTEGER NOT NULL DEFAULT 0,
 explicit INTEGER NOT NULL DEFAULT 0,
 updated_at INTEGER NOT NULL DEFAULT 0,
 updated_by TEXT NOT NULL DEFAULT 'migration'
);
INSERT INTO project_review_mode(project,mode,revision)
 SELECT p.key,CASE WHEN COALESCE(y.enabled,0)=1 THEN 'yolo' ELSE 'custom' END,COALESCE(y.revision,0)
 FROM project p LEFT JOIN project_yolo y ON y.project=p.key;
