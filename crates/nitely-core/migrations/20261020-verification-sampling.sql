-- Server-owned entropy is deliberately excluded from the audit read model.
CREATE TABLE verification_audit_salt(project TEXT PRIMARY KEY, salt BLOB NOT NULL);
CREATE TABLE verification_sample (
 id INTEGER PRIMARY KEY, project TEXT NOT NULL, issue_key TEXT NOT NULL,
 policy TEXT NOT NULL, percent INTEGER NOT NULL CHECK(percent BETWEEN 10 AND 100),
 selected INTEGER NOT NULL, created_at INTEGER NOT NULL,
 UNIQUE(issue_key, policy)
);
CREATE TABLE verification_audit (
 id INTEGER PRIMARY KEY, sample_id INTEGER NOT NULL, project TEXT NOT NULL, repo TEXT NOT NULL,
 issue_key TEXT NOT NULL, base TEXT NOT NULL, head TEXT NOT NULL,
 author TEXT NOT NULL, declarer TEXT, declaration_source TEXT, declared TEXT,
 effective TEXT NOT NULL, selected INTEGER NOT NULL, landed_at INTEGER NOT NULL,
 state TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0, lease_until INTEGER,
 result TEXT, error TEXT, completed_at INTEGER,
 UNIQUE(repo, issue_key, head)
);
CREATE INDEX verification_audit_queue ON verification_audit(project, repo, state, lease_until, id);
CREATE TABLE verification_audit_rule (
 audit_id INTEGER NOT NULL, rule TEXT NOT NULL, PRIMARY KEY(audit_id, rule)
);
CREATE TABLE verification_escape (
 id INTEGER PRIMARY KEY, audit_id INTEGER NOT NULL, rule TEXT NOT NULL,
 source TEXT NOT NULL, evidence TEXT NOT NULL, confirmed_by TEXT NOT NULL,
 confirmed_at INTEGER NOT NULL, UNIQUE(audit_id, rule, source, evidence)
);
CREATE TABLE verification_disabled_rule (
 project TEXT NOT NULL, rule TEXT NOT NULL, escape_id INTEGER NOT NULL,
 disabled_at INTEGER NOT NULL, PRIMARY KEY(project, rule)
);
-- Nitely's legacy database does not enable foreign-key cascades. Keep these records
-- with their owning issues/projects, including renames and permanent removal.
CREATE TRIGGER verification_audit_issue_rekey AFTER UPDATE OF key ON issue BEGIN
 UPDATE verification_sample SET issue_key=NEW.key WHERE issue_key=OLD.key;
 UPDATE verification_audit SET issue_key=NEW.key WHERE issue_key=OLD.key;
END;
CREATE TRIGGER verification_audit_project_rekey AFTER UPDATE OF key ON project BEGIN
 UPDATE verification_audit_salt SET project=NEW.key WHERE project=OLD.key;
 UPDATE verification_sample SET project=NEW.key WHERE project=OLD.key;
 UPDATE verification_audit SET project=NEW.key WHERE project=OLD.key;
 UPDATE verification_disabled_rule SET project=NEW.key WHERE project=OLD.key;
END;
CREATE TRIGGER verification_audit_issue_delete AFTER DELETE ON issue BEGIN
 DELETE FROM verification_audit_rule WHERE audit_id IN (SELECT id FROM verification_audit WHERE issue_key=OLD.key);
 DELETE FROM verification_escape WHERE audit_id IN (SELECT id FROM verification_audit WHERE issue_key=OLD.key);
 DELETE FROM verification_audit WHERE issue_key=OLD.key;
 DELETE FROM verification_sample WHERE issue_key=OLD.key;
END;
CREATE TRIGGER verification_audit_project_delete AFTER DELETE ON project BEGIN
 DELETE FROM verification_audit_salt WHERE project=OLD.key;
 DELETE FROM verification_disabled_rule WHERE project=OLD.key;
END;
