-- Notices the person dismissed in the Inbox (NIT-570): the event that said it, and when.
CREATE TABLE notice_dismissed (event_id INTEGER PRIMARY KEY, dismissed_at INTEGER NOT NULL);
