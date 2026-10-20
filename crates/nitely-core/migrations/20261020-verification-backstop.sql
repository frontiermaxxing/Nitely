CREATE TABLE verification_health (
 repo TEXT PRIMARY KEY, project TEXT NOT NULL, observed_head TEXT NOT NULL,
 verified_head TEXT, pending_since INTEGER NOT NULL, reduced_count INTEGER NOT NULL DEFAULT 0,
 checked_at INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'pending',
 message TEXT NOT NULL DEFAULT '', recovery_issue TEXT
);
CREATE TABLE verification_backstop_run (
 id INTEGER PRIMARY KEY, repo TEXT NOT NULL, head TEXT NOT NULL, base TEXT,
 policy TEXT NOT NULL, commands TEXT NOT NULL, started_at INTEGER NOT NULL,
 ended_at INTEGER, state TEXT NOT NULL, lane_run INTEGER, evidence TEXT NOT NULL DEFAULT '', culprit TEXT,
 isolation_complete INTEGER NOT NULL DEFAULT 0, isolate_failure INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE verification_landing (
 repo TEXT NOT NULL, head TEXT NOT NULL, base TEXT NOT NULL, issue_key TEXT NOT NULL,
 reduced INTEGER NOT NULL, landed_at INTEGER NOT NULL, covered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(repo, head, issue_key)
);
-- The token is allocated before filing. The trigger links the issue in the SAME
-- transaction that creates it, including a crash before create_issue returns.
CREATE TABLE verification_recovery (
 repo TEXT NOT NULL, head TEXT NOT NULL, project TEXT NOT NULL,
 token TEXT NOT NULL UNIQUE, issue_key TEXT, submitted_head TEXT, PRIMARY KEY(repo, head)
);
CREATE TRIGGER verification_recovery_issue_created AFTER INSERT ON issue BEGIN
 UPDATE verification_recovery SET issue_key=NEW.key
 WHERE issue_key IS NULL AND project=NEW.project
 AND instr(NEW.body, '[verification-recovery:' || token || ']') > 0;
END;
CREATE TRIGGER verification_backstop_issue_rekey AFTER UPDATE OF key ON issue BEGIN
 UPDATE verification_recovery SET issue_key=NEW.key WHERE issue_key=OLD.key;
 UPDATE verification_health SET recovery_issue=NEW.key WHERE recovery_issue=OLD.key;
 UPDATE verification_landing SET issue_key=NEW.key WHERE issue_key=OLD.key;
END;
CREATE TRIGGER verification_backstop_project_rekey AFTER UPDATE OF key ON project BEGIN
 UPDATE verification_recovery SET project=NEW.key WHERE project=OLD.key;
 UPDATE verification_health SET project=NEW.key WHERE project=OLD.key;
END;
CREATE TRIGGER verification_backstop_project_delete AFTER DELETE ON project BEGIN
 DELETE FROM verification_backstop_run WHERE repo IN (SELECT repo FROM verification_health WHERE project=OLD.key);
 DELETE FROM verification_landing WHERE repo IN (SELECT repo FROM verification_health WHERE project=OLD.key);
 DELETE FROM verification_recovery WHERE project=OLD.key;
 DELETE FROM verification_health WHERE project=OLD.key;
END;
