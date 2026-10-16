// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).
// The cases live in crates/nitely-core/src/track_cases.json, which track.rs runs too: the
// browser preview's computeTrack and the backend's track_of must agree on every one.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { computeTrack, describeTrack, stationOfState, trackOf, type Station, type Track } from "./lifecycle.ts";
import type { IssueRecord, TimelineEntry } from "../types.ts";

interface Case {
  name: string;
  state: string;
  base?: boolean;
  events: string[][];
  change_reviews?: number;
  /** A rule track.rs has and computeTrack does not yet; reported as a TODO, not a failure. */
  frontend_pending?: string;
  expect: {
    station?: Station;
    stuck?: string | null;
    closed?: string | null;
    rounds?: number;
    asking?: boolean;
    backlog?: boolean;
    since?: number | null;
    reached?: Station[];
    skipped?: Station[];
  };
}

// npm runs tests from the repo root; the compiled file lives elsewhere, so the path is from there.
const { cases } = JSON.parse(readFileSync("crates/nitely-core/src/track_cases.json", "utf8")) as { cases: Case[] };

/** The same prefix as BASE in track.rs. */
const BASE = [
  ["issue_created", "Filed as Todo"],
  ["claimed", "Took the issue"],
  ["issue_moved", "Todo → In Progress"],
  ["worktree", "Started x from main"],
  ["status", "Working"],
  ["gate_passed", "Gate passed"],
  ["review_started", "Review by codex started"],
];

const STATE_TYPE: Record<string, string> = {
  Backlog: "backlog",
  Todo: "unstarted",
  Done: "completed",
  Canceled: "canceled",
  Duplicate: "canceled",
};

function issue(state: string, track?: Track): IssueRecord {
  return {
    key: "X-1",
    origin: "nitely",
    project: "X",
    title: "x",
    body: "",
    url: "",
    state,
    state_type: STATE_TYPE[state] ?? "started",
    kind: null,
    source: null,
    priority: 0,
    labels: [],
    evidence: "",
    claimed_by: null,
    created_at: "",
    updated_at: "",
    blocked_by: [],
    blocks: [],
    urgency: 0,
    track,
  };
}

test("the shared cases file has the cases NIT-34 asked for", () => {
  assert.ok(cases.length >= 20);
  for (const name of [
    "changes asked goes back to Working",
    "a second review is at Review",
    "a stopped land is at Landing even when the issue stays In Review",
    "recovery after a stopped land clears it",
    "a move with evidence reaches Done",
    "a failed resubmit while In Review is stuck at Review",
  ]) {
    assert.ok(cases.some((c) => c.name === name), name);
  }
});

for (const c of cases) {
  test(c.name, { todo: c.frontend_pending }, () => {
    const events = [...(c.base ? BASE : []), ...c.events];
    const timeline: TimelineEntry[] = events.map(([kind, summary, actor], n) => ({
      ts: n + 1,
      actor: actor ?? "claude:x",
      kind,
      summary: summary || null,
    }));
    const t = computeTrack(issue(c.state), timeline, Array.from({ length: c.change_reviews ?? 0 }, () => ({ verdict: "changes" }) as never));
    const ex = c.expect;
    if (ex.station !== undefined) assert.equal(t.station, ex.station, "station");
    if (ex.stuck !== undefined) assert.equal(t.stuck, ex.stuck, "stuck");
    if (ex.closed !== undefined) assert.equal(t.closed, ex.closed, "closed");
    if (ex.rounds !== undefined) assert.equal(t.rounds, ex.rounds, "rounds");
    if (ex.asking !== undefined) assert.equal(t.asking, ex.asking, "asking");
    if (ex.backlog !== undefined) assert.equal(t.backlog, ex.backlog, "backlog");
    if (ex.since !== undefined) assert.equal(t.since ?? null, ex.since, "since");
    for (const s of ex.reached ?? []) assert.ok(t.reached[s] !== undefined, `reached ${s}`);
    for (const s of ex.skipped ?? []) assert.equal(t.reached[s], undefined, `skipped ${s}`);
  });
}

test("trackOf returns the backend's track when the issue carries one", () => {
  const fromBackend: Track = { station: "gate", reached: { gate: 5 }, rounds: 2, stuck: "Gate failed", asking: false, closed: null, backlog: false };
  assert.equal(trackOf(issue("In Progress", fromBackend)), fromBackend);
  assert.equal(trackOf(issue("In Progress")).station, "working");
});

test("states map to stations", () => {
  assert.equal(stationOfState("Needs You"), "needs");
  assert.equal(stationOfState("Something new", "completed"), "done");
  assert.equal(stationOfState("Something new", "started"), "working");
});

test("hover text names the station, rounds, asks and why it is stuck", () => {
  const t: Track = { station: "review", reached: {}, rounds: 2, stuck: "Gate failed", asking: true, closed: null, backlog: false };
  assert.equal(describeTrack(t), "Review · 2 rounds of changes · asked you something · stuck: Gate failed");
});
