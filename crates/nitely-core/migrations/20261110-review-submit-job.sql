-- The submit job whose run recorded a review (NIT-777), so a request to accept one of its
-- findings can tell whether a newer submit has run since. Null for a review recorded outside a
-- job (a land's review, a batch's review) and for reviews recorded before this column.
ALTER TABLE review ADD COLUMN submit_job INTEGER;
