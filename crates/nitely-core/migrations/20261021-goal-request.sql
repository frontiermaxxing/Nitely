-- A "Plan a goal" request the person made in the app, recorded when they made it (NIT-550). It is
-- the proof a plan was asked for: labels and titles can change later, and no agent can add a row.
CREATE TABLE goal_request (
 issue_key TEXT PRIMARY KEY COLLATE NOCASE,
 asked_at INTEGER NOT NULL
);
