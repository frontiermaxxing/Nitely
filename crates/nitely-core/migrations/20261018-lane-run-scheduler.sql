-- Which scheduler a lane request waits under (NIT-325): a newer waiter passes over a request an
-- older nitely queued, which orders the queue its own way, instead of waiting behind it for good.
ALTER TABLE lane_run ADD COLUMN scheduler INTEGER;
