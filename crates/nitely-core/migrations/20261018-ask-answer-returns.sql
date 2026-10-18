-- An answer goes back to the work that waited on it (NIT-277): the state `ask` moved its issue from
-- when it put it in Needs You, so the answer can move it back, and when the answer reached the
-- agent that asked, as a notice on its next nitely call. Answers given before this reached their
-- agents through get_answer or not at all, and are not told again.
ALTER TABLE ask ADD COLUMN moved_from TEXT;
ALTER TABLE ask ADD COLUMN delivered_at INTEGER;
UPDATE ask SET delivered_at = answered_at WHERE answer IS NOT NULL;
