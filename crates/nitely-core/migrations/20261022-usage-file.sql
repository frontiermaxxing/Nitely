-- What each agent transcript gave so far (NIT-628): the byte offset read up to and the token
-- records found before it, so a new process reads only what a file appended since. `format` is the
-- reader's version: a reader that parses differently reads every file again once.
CREATE TABLE usage_file (
 path TEXT PRIMARY KEY,
 format INTEGER NOT NULL,
 offset INTEGER NOT NULL,
 state TEXT NOT NULL,
 read_at INTEGER NOT NULL
);
