-- What a running land queue run is doing now (NIT-218): "stacking", "gating the stack",
-- "bisecting: gating 2 of 3", "landing". The app's board row shows it; NULL once nothing runs.
ALTER TABLE land_run ADD COLUMN step TEXT;
