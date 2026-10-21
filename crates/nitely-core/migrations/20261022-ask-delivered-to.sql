-- Who an ask's answer reached (NIT-547): the agent that asked, or, when it had gone, the agent
-- that took the issue up next. NULL until it reached someone, and for answers delivered before this.
ALTER TABLE ask ADD COLUMN delivered_to TEXT;
