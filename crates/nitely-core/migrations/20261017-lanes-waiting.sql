-- Large-repo mode (lanes.rs, NIT-201): why a request waits for the person's main checkout, which it
-- runs in only while that checkout is clean and on the integration branch.
ALTER TABLE lane_run ADD COLUMN waiting_for TEXT;
