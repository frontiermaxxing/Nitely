ALTER TABLE review ADD COLUMN solution TEXT;
ALTER TABLE review ADD COLUMN gate TEXT;
ALTER TABLE submit_job ADD COLUMN solution TEXT;
ALTER TABLE submit_job ADD COLUMN solution_patch_id TEXT;
ALTER TABLE review ADD COLUMN branch TEXT;
