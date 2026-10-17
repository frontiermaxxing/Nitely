-- How many lanes the request's repo had when it was queued, so the machine-wide queue (lanes.rs, NIT-199)
-- can tell a repo with a free lane from one whose lanes are all busy.
ALTER TABLE lane_run ADD COLUMN lane_count INTEGER NOT NULL DEFAULT 1;
