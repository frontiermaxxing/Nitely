-- Reduced gates that passed in a lane with every selected test seen to run, by tree and commands,
-- so land reuses one on an unchanged tree instead of running the same checks again (NIT-494).
CREATE TABLE verification_gate_pass (
 repo TEXT NOT NULL,
 tree TEXT NOT NULL,
 commands TEXT NOT NULL,
 lane_run INTEGER NOT NULL,
 recorded_at INTEGER NOT NULL,
 PRIMARY KEY(repo, tree, commands)
);
