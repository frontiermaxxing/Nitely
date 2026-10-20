CREATE TABLE verification_plan (
    issue_key TEXT NOT NULL REFERENCES issue(key),
    head TEXT NOT NULL,
    policy_commit TEXT NOT NULL,
    classifier TEXT NOT NULL,
    plan TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (issue_key, head, policy_commit, classifier)
);
