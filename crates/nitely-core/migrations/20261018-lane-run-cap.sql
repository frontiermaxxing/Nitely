-- The memory cap a lane run was started with (NIT-323), so a new run starts only while the running
-- runs' caps and its own fit above the memory floor.
ALTER TABLE lane_run ADD COLUMN memory_cap INTEGER;
