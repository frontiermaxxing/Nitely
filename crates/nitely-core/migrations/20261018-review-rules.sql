-- What a review saw (NIT-185): the playbook's [review] table it ran under, as JSON, and its diff
-- exactly, positions and context too, so a pass is carried over only to the very same change under
-- the same rules. A patch-id alone is not that: it ignores where each hunk sits.
ALTER TABLE review ADD COLUMN rules TEXT;
ALTER TABLE review ADD COLUMN diff_id TEXT;
