-- The move into Needs You that an ask's wait began with (NIT-364): the event `ask` logged when it
-- put the issue there, or, for a question asked while the issue already waited, the event of that
-- wait. An answer puts the issue back only while that move is still the issue's latest entry into
-- Needs You, so a later wait (an approval) is never an ask's to end.
ALTER TABLE ask ADD COLUMN wait_event INTEGER;
-- An ask still waiting from before this (NIT-277's, which noted only where it moved the issue from)
-- is linked to the move into Needs You its `ask` logged in the same minute.
UPDATE ask SET wait_event = (
  SELECT MAX(e.id) FROM event e
  WHERE e.issue_key = ask.issue_key AND e.type = 'issue_moved'
    AND (e.summary LIKE '% → Needs You' OR e.summary LIKE '% → Needs You:%') AND e.summary NOT LIKE 'Needs You →%'
    AND e.ts BETWEEN ask.asked_at - 60 AND ask.asked_at + 60
)
WHERE moved_from IS NOT NULL AND answer IS NULL AND closed IS NULL;
