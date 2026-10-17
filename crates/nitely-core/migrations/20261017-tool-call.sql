-- Which session made an MCP tool call, as Claude Code's PreToolUse hook saw it (NIT-194): the call
-- reaches the nitely MCP server carrying only its tool use id, and acts as that session rather
-- than as every session in its folder. Rows older than a day are dropped as new ones come.
CREATE TABLE tool_call (
  tool_use_id TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL,
  ts          INTEGER NOT NULL
);
CREATE INDEX tool_call_ts ON tool_call (ts);
