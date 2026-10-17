-- A review finding the person accepted as it is (NIT-191, docs/approvals.md), signed like an
-- approval: by the app's approval key, or by a paired phone (device, signed_at).
CREATE TABLE accepted_finding (
  id        INTEGER PRIMARY KEY,
  issue_key TEXT NOT NULL,
  patch_id  TEXT NOT NULL,        -- the patch of the review it was accepted on
  priority  INTEGER NOT NULL,
  finding   TEXT NOT NULL,        -- the finding's text, as that review recorded it
  reason    TEXT NOT NULL,        -- the person's one line
  ts        INTEGER NOT NULL,
  signature TEXT NOT NULL,
  device    TEXT NOT NULL DEFAULT '',
  signed_at INTEGER
);
-- An agent's request that the person accept one finding (JSON {priority, text}); NULL for a plain ask.
ALTER TABLE ask ADD COLUMN finding TEXT;
