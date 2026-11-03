-- What the app reads every time the database changes, by index instead of by scanning (NIT-730).
-- Without them each read scanned the review table once per issue (its rows carry findings and
-- notes), the week's events to pick out a few types, every event to find an agent's last one, and
-- the issues once per goal: gigabytes a minute from a database of a few hundred MB.
CREATE INDEX review_issue ON review (issue_key COLLATE NOCASE, ts, id, verdict);
CREATE INDEX event_type_ts ON event (type, ts);
CREATE INDEX event_actor_ts ON event (actor, ts);
CREATE INDEX issue_goal ON issue (goal);
CREATE INDEX lane_run_state ON lane_run (state);
