-- A submit job run by a process of its own (`nitely submit-job <id>`), not in its caller after a
-- process could not be started (NIT-225): its process is checked by that command line.
ALTER TABLE submit_job ADD COLUMN detached INTEGER NOT NULL DEFAULT 0;
