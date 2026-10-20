CREATE TABLE periodic_review_state (
 repo TEXT PRIMARY KEY, project TEXT NOT NULL, baseline TEXT NOT NULL,
 cursor TEXT, observed_head TEXT NOT NULL, last_success_at INTEGER, error TEXT, pending_since INTEGER
);
CREATE TABLE periodic_review_landing (
 repo TEXT NOT NULL, project TEXT NOT NULL, issue_key TEXT NOT NULL,
 base TEXT NOT NULL, head TEXT NOT NULL, landed_at INTEGER NOT NULL,
 PRIMARY KEY(repo,issue_key,head)
);
CREATE TABLE periodic_review_job (
 id INTEGER PRIMARY KEY, repo TEXT NOT NULL, project TEXT NOT NULL,
 base TEXT NOT NULL, head TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending',
 attempt INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
 completed_at INTEGER, lease_until INTEGER, retry_at INTEGER NOT NULL DEFAULT 0,
 reviewer TEXT, result TEXT, error TEXT,
 UNIQUE(repo,base,head)
);
CREATE UNIQUE INDEX periodic_review_one_active ON periodic_review_job(repo)
 WHERE state IN ('pending','running','applying','failed');
CREATE TABLE periodic_review_finding (
 job INTEGER NOT NULL REFERENCES periodic_review_job(id), fingerprint TEXT NOT NULL,
 priority INTEGER NOT NULL, text TEXT NOT NULL, issue_key TEXT, action TEXT NOT NULL DEFAULT 'pending',
 PRIMARY KEY(job,fingerprint)
);
CREATE INDEX periodic_review_history ON periodic_review_job(project,repo,id DESC);
CREATE INDEX periodic_review_dedup ON periodic_review_finding(fingerprint,issue_key);
