-- A lane run's peak once its first heavy phase (compiling) was over and its working set had
-- dropped (NIT-516): what its tests took, apart from `peak_memory`, which is the whole run's.
ALTER TABLE lane_run ADD COLUMN test_peak_memory INTEGER;
