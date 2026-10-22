-- The gate step that ran each recorded test (NIT-565): two packages may each have a test of one
-- name, and their records are counted apart by (test, step). Rows from before are ''.
ALTER TABLE test_catch ADD COLUMN step TEXT NOT NULL DEFAULT '';
