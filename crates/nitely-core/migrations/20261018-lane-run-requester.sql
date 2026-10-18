-- Who asked for a lane run, beyond its pid (NIT-270): the requester's start time, so a pid the OS
-- has given to another process since is not taken for the requester, and when the requester last
-- looked for its turn, so a queued request whose requester stopped looking holds nobody up.
ALTER TABLE lane_run ADD COLUMN pid_started TEXT;
ALTER TABLE lane_run ADD COLUMN polled_at INTEGER;
