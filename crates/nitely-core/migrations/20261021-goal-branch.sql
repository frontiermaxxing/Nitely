CREATE TABLE goal_branch (
 goal_id INTEGER PRIMARY KEY,
 repo TEXT NOT NULL,
 branch TEXT NOT NULL,
 created_at INTEGER,
 created_from TEXT,
 state TEXT NOT NULL DEFAULT 'green' CHECK(state IN ('green','red')),
 red_reason TEXT,
 red_main TEXT,
 red_issue TEXT,
 checkpoint_items INTEGER NOT NULL DEFAULT 10,
 checkpoint_head TEXT,
 checkpoint_at INTEGER,
 enabled_by TEXT NOT NULL,
 enabled_at INTEGER NOT NULL,
 closed_at INTEGER
);
CREATE TABLE goal_branch_item (
 id INTEGER PRIMARY KEY,
 goal_id INTEGER NOT NULL,
 issue_key TEXT NOT NULL,
 from_sha TEXT NOT NULL,
 to_sha TEXT NOT NULL,
 landed_at INTEGER NOT NULL,
 checkpoint_id INTEGER,
 reverted_at INTEGER
);
CREATE INDEX goal_branch_item_goal ON goal_branch_item(goal_id, checkpoint_id);
CREATE TABLE goal_checkpoint (
 id INTEGER PRIMARY KEY,
 goal_id INTEGER NOT NULL,
 issue_key TEXT NOT NULL,
 head TEXT NOT NULL,
 state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open','landed','sent_back')),
 opened_at INTEGER NOT NULL,
 ended_at INTEGER,
 gates INTEGER NOT NULL DEFAULT 0,
 note TEXT
);
CREATE INDEX goal_checkpoint_issue ON goal_checkpoint(issue_key);
