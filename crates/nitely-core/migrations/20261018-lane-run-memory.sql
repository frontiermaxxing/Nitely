-- Memory admission (machine.rs, NIT-266): why a request waits for the machine's memory, and the
-- most memory its processes held at once.
ALTER TABLE lane_run ADD COLUMN waiting_memory TEXT;
ALTER TABLE lane_run ADD COLUMN peak_memory INTEGER;
