-- Where each reader of the updates feed has read up to (NIT-562): an agent session's id, or a name
-- a script picks, and the last event it was told about.
CREATE TABLE update_cursor (reader TEXT PRIMARY KEY, event_id INTEGER NOT NULL, read_at INTEGER NOT NULL);
