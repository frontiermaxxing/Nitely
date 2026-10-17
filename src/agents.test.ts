// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { agentLabel, agentTitle } from "./ui/agents.ts";

test("a session is named by its vendor and the end of its id, the same everywhere", () => {
  assert.equal(agentLabel("claude:60f2a6c3-5858-4141-b8bb-a52c02f13317"), "Claude · 3317");
  assert.equal(agentLabel("codex:01a146fa-40c8-7393-8fc6-f768a3dfb9a2"), "Codex · b9a2");
  assert.notEqual(agentLabel("claude:60f2a6c3-5858-4141-b8bb-a52c02f13317"), agentLabel("claude:7ac581bb-e11d-4c95-b25d-cc3d52f43578"));
});

test("an agent known only by its folder keeps the folder's name", () => {
  assert.equal(agentLabel("claude:c:/code/my-app"), "Claude · my-app");
  assert.equal(agentLabel("codex:/home/sam/code/my-app-worktrees/app-3-fix"), "Codex · app-3-fix");
  assert.equal(agentLabel("cli"), "Cli");
});

test("the hover says the full id and where the session works", () => {
  assert.equal(agentTitle("claude:60f2a6c3-5858-4141-b8bb-a52c02f13317", "C:\\Code\\my-app"), "Claude · 3317: claude:60f2a6c3-5858-4141-b8bb-a52c02f13317, in C:\\Code\\my-app");
  assert.equal(agentTitle("claude:c:/code/my-app"), "Claude · my-app: claude:c:/code/my-app");
});
