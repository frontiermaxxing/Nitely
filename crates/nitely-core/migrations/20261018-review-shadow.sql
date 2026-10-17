-- Shadow batches (NIT-217): a small change is reviewed alone, which decides, and also in a batch
-- that is only recorded, to compare the two. A review's seconds and tokens, to compare their cost.
ALTER TABLE review_queue ADD COLUMN shadow INTEGER NOT NULL DEFAULT 0;
ALTER TABLE review_queue ADD COLUMN review_id INTEGER;  -- shadow: the single review that decided
ALTER TABLE review_batch ADD COLUMN shadow INTEGER NOT NULL DEFAULT 0;
ALTER TABLE review_batch ADD COLUMN tokens INTEGER;
ALTER TABLE review ADD COLUMN seconds INTEGER;
ALTER TABLE review ADD COLUMN tokens INTEGER;
