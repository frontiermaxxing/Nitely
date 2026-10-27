-- A disputed P0/P1 finding gets a fresh review before it reaches the person (NIT-318): the triage
-- question that checks it carries the finding and the agent's reasoning, as JSON.
ALTER TABLE triage ADD COLUMN finding TEXT;
