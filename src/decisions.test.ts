// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { askPlaces, decisionsOf, issueDecision, firstLine, outcomeOf, shownDecisions, trackAfter, type Decision, type Resolved } from "./decisions.ts";
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

test("NIT-172: questions sharing an issue are told apart by their own first line and their place", () => {
  const asks = [ask(3, "X-1", NOW - 10), ask(1, "X-1", NOW - 30), ask(2, "X-1", NOW - 20), ask(4, "X-2", NOW - 5)];
  const ds = decisionsOf(board([], asks), new Map(), NOW);
  const places = askPlaces(ds);
  assert.deepEqual(places.get("ask-1"), { n: 1, of: 3 });
  assert.deepEqual(places.get("ask-3"), { n: 3, of: 3 });
  assert.equal(places.has("ask-4"), false, "a question alone on its issue needs no number");
  assert.equal(firstLine("\n  Which currency should prices show?\nWe sell in the EU and the US."), "Which currency should prices show?");
  assert.equal(firstLine("   "), "");
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

test("NIT-220: review findings waiting on the person are the issue's only decisions: no approval, no work to mark done", () => {
  const stalled = issue("X-5", "Needs You");
  const request = (id: number, text: string): Ask => ({ ...ask(id, "X-5", NOW - 60), question: "The review rounds are not converging.", options: [], finding: { priority: 1, text } });
  const ds = decisionsOf(board([stalled], [request(7, "Drops a row: a.rs"), request(8, "Leaks a key: b.rs")]), new Map(), NOW);
  assert.deepEqual(
    ds.map((d) => [d.id, d.kind]),
    [
      ["ask-7", "ask"],
      ["ask-8", "ask"],
    ],
  );
  // Also for an issue an older submit made the person's own: its findings are what they decide.
  const own = { ...stalled, owner: "human" };
  assert.deepEqual(decisionsOf(board([own], [request(7, "Drops a row: a.rs")]), new Map(), NOW).map((d) => d.kind), ["ask"]);
  // A plain question on an issue that waits for approval leaves the approval in place.
  assert.deepEqual(decisionsOf(board([stalled], [ask(9, "X-5", NOW - 30)]), new Map(), NOW).map((d) => d.kind).sort(), ["approve", "ask"]);
});

test("NIT-227: a pending playbook change or goal plan is its own decision, never work to mark done", () => {
  const proposalIssue = { ...issue("X-6", "Needs You"), owner: "human" };
  const pending = { ...detail(proposalIssue), proposal: { status: "pending" } } as unknown as IssueDetail;
  assert.equal(issueDecision(proposalIssue, pending), "playbook");
  const plan = { ...detail(proposalIssue), goal_plan: { status: "pending" } } as unknown as IssueDetail;
  assert.equal(issueDecision(proposalIssue, plan), "goal-plan");
  // Decided (applied), it is no longer a playbook decision; with no detail yet it reads as the owner says.
  const applied = { ...detail(proposalIssue), proposal: { status: "applied" } } as unknown as IssueDetail;
  assert.equal(issueDecision(proposalIssue, applied), "own");
  assert.equal(issueDecision(issue("X-7", "Needs You")), "approve");
  assert.equal(issueDecision(issue("X-8", "Todo")), null);
  // The Inbox lists it as the same decision: an approval card that shows the change, not own work.
  const ds = decisionsOf(board([proposalIssue]), new Map([["X-6", pending]]), NOW);
  assert.deepEqual(ds.map((d) => [d.id, d.kind]), [["approve-X-6", "approve"]]);
});

test("the human's own work is its own kind of decision, in any open state outside Backlog", () => {
  const own = (state: string) => ({ ...issue("X-9", state), owner: "human" });
  assert.deepEqual(
    decisionsOf(board([own("Needs You")]), new Map(), NOW).map((d) => [d.id, d.kind]),
    [["own-X-9", "own"]],
  );
  assert.equal(decisionsOf(board([own("Todo")]), new Map(), NOW)[0]?.kind, "own");
  assert.equal(decisionsOf(board([own("Backlog")]), new Map(), NOW).length, 0);
  assert.equal(decisionsOf(board([{ ...own("Done"), state_type: "completed" }]), new Map(), NOW).length, 0);
});

test("marking your own work done or handing it back moves the track at once", () => {
  const t: Track = { station: "needs", reached: { todo: 1, needs: 2 }, rounds: 0, stuck: null, asking: false, closed: null, backlog: false, since: 2 };
  assert.equal(trackAfter(t, "done", NOW).station, "done");
  assert.equal(trackAfter(t, "handback", NOW).station, "todo");
});

test("a fresh rebase conflict waits from when the rebase stopped, not from the branch's last commit", () => {
  const row = {
    worktree: "C:/code/app-worktrees/x-7",
    branch: "claude/x-7-y",
    is_main: false,
    vendor: "claude",
    issue_key: "X-7",
    state: { ahead: 2, behind: 9, dirty_files: 1, last_commit_subject: "old work", last_commit_at: NOW - 5 * 86400, rebase_in_progress: true, rebase_started_at: NOW - 120 },
    error: null,
    session: null,
    issue: issue("X-7", "In Progress"),
    pr: null,
    needs_me: true,
  };
  const b: Board = { ...board([]), repos: [{ name: "app", root: "C:/code/app", integration_branch: "main", project: "X", rows: [row], branches: [], error: null, unmatched_sessions: [] }] } as unknown as Board;
  const [d] = decisionsOf(b, new Map(), NOW);
  assert.equal(d.kind, "conflict");
  assert.equal(d.since, NOW - 120);
  // Without the rebase's time (an older backend), the last commit is the best guess left.
  const old = { ...row, state: { ...row.state, rebase_started_at: null } };
  const b2 = { ...b, repos: [{ ...b.repos[0], rows: [old] }] } as unknown as Board;
  assert.equal(decisionsOf(b2, new Map(), NOW)[0].since, NOW - 5 * 86400);
});
