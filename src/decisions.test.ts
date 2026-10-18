// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { APPROVALS, askPlaces, decisionsOf, folderOf, sessionOf, groupApprovals, inProject, issueDecision, keyProject, nextProject, projectCounts, projectOf, firstLine, outcomeOf, shownDecisions, trackAfter, type Decision, type Resolved } from "./decisions.ts";
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
  return { repos: [], needs_me_issues: needsMe, asks, sync: [], remote_sessions: [], agents: [] };
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

test("NIT-218: several changes waiting for approval are one card, each still its own decision", () => {
  const a = issue("X-1", "Needs You", 3);
  const b = issue("X-2", "Needs You", 2);
  const c = issue("X-3", "Needs You", 3);
  const proposal = { ...issue("X-4", "Needs You"), owner: "human" };
  const asked = ask(9, "X-5", NOW - 100);
  const details = new Map<string, IssueDetail>([
    ["X-1", detail(a)],
    ["X-2", detail(b)],
    ["X-4", { ...detail(proposal), proposal: { status: "pending" } } as unknown as IssueDetail],
  ]);
  const ds = decisionsOf(board([a, b, c, proposal], [asked]), details, NOW);
  const shown = groupApprovals(ds, details);
  const group = shown.find((d) => d.id === APPROVALS)!;
  // X-3's details are not read yet (it could be a playbook change), and X-4 is one: both on their own cards.
  assert.deepEqual(group.group!.map((d) => d.id), ["approve-X-2", "approve-X-1"], "most urgent first");
  assert.equal(group.urgency, 2);
  assert.deepEqual(shown.filter((d) => d !== group).map((d) => d.id).sort(), ["approve-X-3", "approve-X-4", "ask-9"]);
  // One change alone is its own card, as before; one just approved stays in the card meanwhile.
  assert.deepEqual(groupApprovals(ds, new Map([["X-1", detail(a)]])), ds);
  const approved = { ...a, state: "Approved" };
  const after = new Map(details).set("X-1", detail(approved));
  assert.deepEqual(groupApprovals(ds, after, new Set(["approve-X-1"])).find((d) => d.id === APPROVALS)!.group!.map((d) => d.id), ["approve-X-2", "approve-X-1"]);
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

test("NIT-236: the Inbox filters by project, its counts add up, and grouped approvals follow the filter", () => {
  const inApp = (key: string, state = "Needs You") => ({ ...issue(key, state), project: "APP" });
  const inWeb = (key: string) => ({ ...issue(key, "Needs You"), project: "WEB" });
  const issues = [inApp("APP-1"), inApp("APP-2"), inWeb("WEB-1"), inApp("APP-3")];
  // A question on an issue the board does not list still belongs to its key's project.
  const asks = [ask(1, "WEB-7", NOW - 50), { ...ask(2, "APP-9", NOW - 40), issue_key: null }];
  const details = new Map(issues.map((i) => [i.key, detail(i)] as [string, IssueDetail]));
  const ds = decisionsOf(board(issues, asks), details, NOW);
  assert.deepEqual(projectCounts(ds), [
    { project: "APP", count: 3 },
    { project: "WEB", count: 2 },
  ]);
  assert.equal(ds.find((d) => d.id === "ask-2")!.project, null, "a question on no issue is in no project: only All shows it");
  assert.deepEqual(inProject(ds, "web").map((d) => d.id).sort(), ["approve-WEB-1", "ask-1"]);
  assert.equal(inProject(ds, null).length, ds.length);
  // Grouped within the project filtered on: WEB's one approval is its own card, APP's three are one.
  assert.equal(groupApprovals(inProject(ds, "WEB"), details).find((d) => d.id === APPROVALS), undefined);
  const app = groupApprovals(inProject(ds, "APP"), details).find((d) => d.id === APPROVALS)!;
  assert.equal(app.group!.length, 3);
  assert.equal(projectOf(app), "APP");
  assert.equal(projectOf(groupApprovals(ds, details).find((d) => d.id === APPROVALS)!), null, "a card mixing projects is in none");
  // Cycling: All, then each project in the chips' order, then All again.
  const counts = projectCounts(ds);
  assert.deepEqual([nextProject(counts, null, 1), nextProject(counts, "APP", 1), nextProject(counts, "WEB", 1), nextProject(counts, null, -1)], ["APP", "WEB", null, "WEB"]);
  assert.equal(nextProject(counts, "GONE", 1), "APP", "a project with nothing left cycles from All");
  assert.deepEqual([keyProject("nit-12"), keyProject("feature/x"), keyProject(null)], ["NIT", null, null]);
});

test("NIT-273: each session waiting at its prompt is its own decision, however many share a folder", () => {
  const session = (id: string, state: "waiting" | "working") => ({ id, vendor: "claude", cwd: "C:/code/app", started_at: NOW - 600, last_seen_at: NOW - 60, last_activity: null, state });
  const agent = (id: string, key: string, state: "waiting" | "working") => ({ session: session(id, state), repo: "app", project: "X", worktree: null, issue_key: key, issue: issue(key, "In Progress"), report: null, needs_me: state === "waiting" });
  // Three from one main checkout: two wait at their prompt, one of them on a question it asked.
  const asked = { ...ask(9, "X-3", NOW - 60), asked_by: "claude:c3" };
  const b: Board = { ...board([], [asked]), agents: [agent("claude:c1", "X-1", "waiting"), agent("claude:c2", "X-2", "working"), agent("claude:c3", "X-3", "waiting")] } as unknown as Board;
  const ds = decisionsOf(b, new Map(), NOW);
  assert.deepEqual(ds.map((d) => [d.kind, d.issue?.key ?? d.ask?.issue_key]).sort(), [["ask", "X-3"], ["input", "X-1"]]);
  // With no task row, the decision still names its own session and folder.
  const input = ds.find((d) => d.kind === "input")!;
  assert.equal(sessionOf(input)?.id, "claude:c1");
  assert.equal(folderOf(input), "C:/code/app");
});
