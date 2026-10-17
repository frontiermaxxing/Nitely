// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { crewChanged, crewDraftOf, crewEditOf, crewTitle, crewWords } from "./crew.ts";
import type { CrewStatus, CrewView } from "./types.ts";

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
  const w = { id: 1, project: "NIT", issue_key: "NIT-3", agent: "codex", pid: 9, worktree: "/w", log: "/l", started_at: 1000, ended_at: null, exit_code: null, outcome: null, reason: null, phase: "work" };
  assert.equal(crewTitle({ ...base, running: [w] }, 1000 + 600), "NIT-3 · codex · started 10m ago\n2 of 20 started today");
});

const view: CrewView = {
  repo: "shop", root: "C:\\Code\\shop", source: "stored", path: "C:\\data\\playbooks\\shop.toml", integration: "main",
  workers: 0, agents: ["claude", "codex"], max_per_day: null, default_max_per_day: 20, max_tokens_per_day: null, quiet_hours: null,
  choices: [{ name: "claude", installed: false }, { name: "codex", installed: true }], has_gate: true, pending_proposal: null,
};

test("a crew that is off drafts the installed agents, and saves nothing until something changes", () => {
  const d = crewDraftOf(view);
  assert.deepEqual(d.agents, ["claude", "codex"], "agents the playbook names stay as they are");
  assert.deepEqual(crewDraftOf({ ...view, agents: [] }).agents, ["codex"], "with none named, the installed ones");
  assert.equal(crewChanged(view, d), false);
  assert.equal(crewChanged(view, { ...d, workers: 2 }), true);
});

test("the draft saves the playbook's units and refuses what the crew could not run", () => {
  const on = { ...crewDraftOf(view), workers: 2, maxPerDay: "10", budgetMillions: "5.5", quiet: true, quietFrom: "23:00", quietTo: "06:30" };
  assert.deepEqual(crewEditOf(on), { workers: 2, agents: ["claude", "codex"], max_per_day: 10, max_tokens_per_day: 5_500_000, quiet_hours: "23:00-06:30" });
  const saved = { ...view, workers: 2, max_per_day: 10, max_tokens_per_day: 5_500_000, quiet_hours: "23:00-06:30" };
  assert.deepEqual(crewDraftOf(saved), { ...on, agents: ["claude", "codex"] });
  assert.equal(crewChanged(saved, crewDraftOf(saved)), false);
  assert.match(crewEditOf({ ...on, agents: [] }) as string, /at least one agent/);
  assert.match(crewEditOf({ ...on, maxPerDay: "0" }) as string, /at least 1/);
  assert.match(crewEditOf({ ...on, budgetMillions: "lots" }) as string, /millions/);
  assert.match(crewEditOf({ ...on, quietTo: "23:00" }) as string, /same time/);
});
