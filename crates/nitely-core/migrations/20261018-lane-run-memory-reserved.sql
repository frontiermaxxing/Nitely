-- What admission set aside for a lane run (NIT-363): its command's expected peak (p90 of its past
-- peaks plus a quarter, at least 2 GB, the full cap without history), not the cap it is held to.
ALTER TABLE lane_run ADD COLUMN memory_reserved INTEGER;
