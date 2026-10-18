-- Whether a lane request's repo has a quick lane (NIT-347): a checkout beyond its [lanes] count,
-- only for quick runs, one at a time. And whether a running quick run holds the machine's one
-- slot beyond its budget, which only a quick run may take.
ALTER TABLE lane_run ADD COLUMN quick_lane INTEGER NOT NULL DEFAULT 0;
ALTER TABLE lane_run ADD COLUMN quick_slot INTEGER NOT NULL DEFAULT 0;
