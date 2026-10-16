// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { crewTitle, crewWords } from "./crew.ts";
import type { CrewStatus } from "./types.ts";

const base: CrewStatus = {
  project: "NIT", cap: 3, running: [], started_today: 2, max_per_day: 20, paused: false, supervised: true, status: "working", recent: [],
};

test("a working crew says nothing beside its count", () => {
  assert.equal(crewWords(base), null);
  assert.equal(crewWords({ ...base, status: "working; nothing else ready" }), null);
});

test("a crew that is not working says why, and asks for a person when only one can fix it", () => {
  assert.deepEqual(crewWords({ ...base, supervised: false }), { text: "not running: start nitely crew run", attention: true });
  assert.deepEqual(crewWords({ ...base, paused: true, status: "paused" }), { text: "paused", attention: false });
  assert.deepEqual(crewWords({ ...base, paused: true, status: "paused: no agent will start (claude: exited 2 times right after starting)" }), {
    text: "paused: no agent will start (claude: exited 2 times right after starting)",
    attention: true,
  });
  assert.deepEqual(crewWords({ ...base, status: "quiet hours until 07:00" }), { text: "quiet hours until 07:00", attention: false });
  assert.deepEqual(crewWords({ ...base, status: "20 of 20 started today" }), { text: "20 of 20 started today", attention: false });
  assert.equal(crewWords({ ...base, status: "no gate: the playbook has no [gate] commands, so the crew does not run" })?.attention, true);
});

test("the hover lists running workers and today's count", () => {
  const w = { id: 1, project: "NIT", issue_key: "NIT-3", agent: "codex", pid: 9, worktree: "/w", log: "/l", started_at: 1000, ended_at: null, exit_code: null, outcome: null, reason: null };
  assert.equal(crewTitle({ ...base, running: [w] }, 1000 + 600), "NIT-3 · codex · started 10m ago\n2 of 20 started today");
});
