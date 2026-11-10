-- Status polls (NIT-765): each submit_status or run_status call an agent makes, by the issue it
-- asked about, so `pipeline` can count them per landed change. Every poll is a model call over the
-- agent's whole context, which waking the agent when its job ends saves.
CREATE TABLE status_poll (
 id INTEGER PRIMARY KEY,
 issue_key TEXT,
 tool TEXT NOT NULL,
 actor TEXT NOT NULL,
 waited INTEGER NOT NULL DEFAULT 0,
 ts INTEGER NOT NULL
);
CREATE INDEX status_poll_issue ON status_poll(issue_key);
