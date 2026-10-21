-- The newest event when a review was recorded (NIT-544): a review is after a decision (an
-- approval, a hand-back, an answer to a finding) when it saw that decision's event, which whole
-- seconds cannot tell when both fall in the same one. NULL for reviews recorded before this.
ALTER TABLE review ADD COLUMN after_event INTEGER;
