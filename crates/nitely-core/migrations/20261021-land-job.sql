CREATE TABLE land_job (
 id INTEGER PRIMARY KEY,
 issue_key TEXT NOT NULL,
 after TEXT NOT NULL DEFAULT '[]',
 dir TEXT NOT NULL,
 requested_by TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('waiting','landing','done','failed','replaced')),
 queued_at INTEGER NOT NULL,
 started_at INTEGER,
 ended_at INTEGER,
 pid INTEGER,
 result TEXT,
 error TEXT,
 delivered_at INTEGER
);
CREATE INDEX land_job_issue ON land_job(issue_key);
