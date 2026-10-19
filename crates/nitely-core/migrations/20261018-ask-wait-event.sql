-- The move into Needs You that an ask's wait began with (NIT-364): the event `ask` logged when it
-- put the issue there, or, for a question asked while the issue already waited, the event of that
-- wait. An answer puts the issue back only while that move is still the issue's latest entry into
-- Needs You, so a later wait (an approval) is never an ask's to end.
ALTER TABLE ask ADD COLUMN wait_event INTEGER;
