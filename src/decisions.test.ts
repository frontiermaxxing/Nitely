// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { decisionsOf, outcomeOf, shownDecisions, trackAfter, type Decision, type Resolved } from "./decisions.ts";
import type { Ask, Board, IssueDetail, IssueRecord } from "./types.ts";
import type { Track } from "./ui/lifecycle.ts";

const NOW = 1_800_000_000;

function issue(key: string, state: string, priority = 3): IssueRecord {
  return {
    key,
    origin: "nitely",
    project: "X",
    title: `Issue ${key}`,
    body: "",
    url: `nitely://issue/${key}`,
    state,
    state_type: state === "Todo" ? "unstarted" : "started",
    kind: "bug",
    source: null,
    priority,
    labels: [],
    evidence: "",
    claimed_by: null,
    created_at: "2026-10-16T10:00:00Z",
    updated_at: "2026-10-16T10:00:00Z",
    blocked_by: [],
    blocks: [],
    urgency: priority,
  };
}

function board(needsMe: IssueRecord[], asks: Ask[] = []): Board {
  return { repos: [], needs_me_issues: needsMe, asks, sync: [], remote_sessions: [] };
}

function detail(i: IssueRecord): IssueDetail {
  return { issue: i, relations: { blocked_by: [], blocks: [] }, timeline: [], prs: [], reviews: [] };
}

const ask = (id: number, key: string, at: number): Ask => ({
  id,
  issue_key: key,
  asked_by: "claude:c:/work/x",
  question: "Which one?",
  options: ["A", "B"],
  answer: null,
  asked_at: at,
  answered_at: null,
});

test("an issue in Needs You is an approval", () => {
  const ds = decisionsOf(board([issue("X-1", "Needs You")]), new Map(), NOW);
  assert.deepEqual(
    ds.map((d) => [d.id, d.kind]),
    [["approve-X-1", "approve"]],
  );
});

test("an issue the human approved is not offered as an approval while the board still says Needs You", () => {
  // The board came from a snapshot that started before the move; the detail read after it.
  const stale = board([issue("X-1", "Needs You"), issue("X-2", "Needs You")]);
  const details = new Map([["X-1", detail(issue("X-1", "Approved"))]]);
  const ds = decisionsOf(stale, details, NOW);
  assert.deepEqual(
    ds.map((d) => d.id),
    ["approve-X-2"],
  );
});

test("an issue sent back to Todo is not offered as an approval either", () => {
  const ds = decisionsOf(board([issue("X-1", "Needs You")]), new Map([["X-1", detail(issue("X-1", "Todo"))]]), NOW);
  assert.equal(ds.length, 0);
});

test("a decision just acted on stays in its place, even once the board has dropped it", () => {
  const b = board([issue("X-1", "Needs You", 1), issue("X-2", "Needs You", 2), issue("X-3", "Needs You", 3)]);
  const all = decisionsOf(b, new Map(), NOW);
  const held: Resolved = { decision: all[1], move: "approve", done: true };
  const resolved = new Map([[all[1].id, held]]);
  const live = all.filter((d) => d.id !== "approve-X-2");
  assert.deepEqual(
    shownDecisions(live, resolved, new Set()).map((d) => d.id),
    ["approve-X-1", "approve-X-2", "approve-X-3"],
  );
  // Held while the board still lists it: it appears once.
  assert.equal(shownDecisions(all, resolved, new Set()).length, 3);
});

test("a decision the Inbox moved on from stays hidden while the board still lists it", () => {
  const all = decisionsOf(board([issue("X-1", "Needs You"), issue("X-2", "Needs You")]), new Map(), NOW);
  assert.deepEqual(
    shownDecisions(all, new Map(), new Set(["approve-X-1"])).map((d) => d.id),
    ["approve-X-2"],
  );
});

test("the card says what happened", () => {
  const [d] = decisionsOf(board([issue("X-1", "Needs You")]), new Map(), NOW);
  assert.equal(outcomeOf({ decision: d, move: "approve" }), "Approved. Landing next.");
  assert.equal(outcomeOf({ decision: d, move: "todo" }), "Sent back to Todo.");
  const [q] = decisionsOf(board([], [ask(7, "X-1", NOW - 60)]), new Map(), NOW) as Decision[];
  assert.equal(outcomeOf({ decision: q, move: "answer", answer: "A" }), "Answered: \u201cA\u201d. Sent to Claude.");
});

test("the track moves to where the move takes it", () => {
  const t: Track = { station: "needs", reached: { todo: 1, started: 2, review: 3, needs: 4 }, rounds: 1, stuck: null, asking: true, closed: null, backlog: false };
  const approved = trackAfter(t, "approve", NOW);
  assert.equal(approved.station, "landing");
  assert.equal(approved.reached.landing, NOW);
  assert.equal(approved.asking, false);
  assert.equal(trackAfter({ ...t, station: "landing", stuck: "Land stopped" }, "todo", NOW).station, "todo");
  assert.equal(trackAfter({ ...t, station: "landing", stuck: "Land stopped" }, "todo", NOW).stuck, null);
  assert.equal(trackAfter(t, "answer", NOW).asking, false);
  assert.equal(trackAfter(t, "answer", NOW).station, "needs");
});
