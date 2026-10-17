// What waits on the human, as decisions: built from the board and, where the board alone cannot
// tell (why an approval waits, whether a land stopped), from issue details. The Inbox draws them.

import type { Ask, Board, IssueDetail, IssueRecord, TaskRow } from "./types";
// Straight from the modules rather than ./ui, so node:test can load this file without the CSS.
import { VENDOR_LABEL, livenessOf, vendorOf } from "./ui/agents.ts";
import { trackOf, type Track } from "./ui/lifecycle.ts";

export type DecisionKind = "ask" | "approve" | "own" | "conflict" | "input" | "land";

export interface Decision {
  /** Stable across refreshes, for React keys and the card's own state. */
  id: string;
  kind: DecisionKind;
  issue: IssueRecord | null;
  /** The issue's worktree row, when one is checked out. */
  row: TaskRow | null;
  repo: string | null;
  ask: Ask | null;
  /** When it started waiting on the human. */
  since: number;
  /** 1 (urgent) to 4 (low); 5 when the issue has no priority or there is no issue. */
  urgency: number;
  /** The Inbox's card for several change approvals at once (`groupApprovals`): each one. */
  group?: Decision[];
}

export const KIND_LABEL: Record<DecisionKind, string> = {
  ask: "question",
  approve: "approval",
  own: "your own work",
  conflict: "conflict",
  input: "waiting agent",
  land: "stopped land",
};

/** Issues whose details the Inbox needs: every issue a decision names, and every issue that could hold a stopped land. */
export function detailKeys(board: Board): string[] {
  const keys = new Set<string>();
  for (const a of board.asks) if (a.issue_key) keys.add(a.issue_key);
  for (const i of board.needs_me_issues) keys.add(i.key);
  for (const r of board.repos.flatMap((r) => r.rows)) {
    if (!r.issue || r.issue.origin !== "nitely") continue;
    // Any issue being worked on can hold a stopped land (its state does not always say so).
    if (r.needs_me || r.issue.state_type === "started" || needsHuman(r.issue) || r.issue.state === "Approved") keys.add(r.issue.key);
  }
  return [...keys].sort();
}

/** The state that waits on a person (nitely-core's NEEDS_HUMAN). */
const NEEDS_HUMAN = "Needs You";
export const needsHuman = (i: IssueRecord) => i.state === NEEDS_HUMAN;
/** Open work only the human can do, outside Backlog: it waits on them whatever its state. */
export const isOwnWork = (i: IssueRecord) => i.owner === "human" && !["completed", "canceled"].includes(i.state_type) && i.state !== "Backlog";

/**
 * What an issue waits on the person for, as the one decision the Inbox and the issue panel both
 * show (NIT-227): a playbook change or a goal plan to decide (their issues are the person's so no
 * agent is handed them, but they are never work to mark done), their own work, a change to
 * approve, or nothing.
 */
export type IssueDecision = "playbook" | "goal-plan" | "own" | "approve" | null;
export function issueDecision(i: IssueRecord, detail?: IssueDetail): IssueDecision {
  if (detail?.proposal?.status === "pending") return "playbook";
  if (detail?.goal_plan?.status === "pending") return "goal-plan";
  if (isOwnWork(i)) return "own";
  return needsHuman(i) ? "approve" : null;
}

const urgencyOf = (i: IssueRecord | null | undefined) => {
  const u = i ? i.urgency || i.priority : 0;
  return u > 0 ? u : 5;
};

const parseTs = (iso: string) => {
  const t = Date.parse(iso);
  return Number.isNaN(t) ? 0 : t / 1000;
};

/** When the issue last moved to Needs You, from its timeline; else when it last changed. */
function waitingSince(issue: IssueRecord, detail?: IssueDetail): number {
  const moved = detail?.timeline.filter((e) => e.kind === "issue_moved" && e.summary?.includes(`→ ${NEEDS_HUMAN}`)).pop();
  return moved?.ts ?? parseTs(issue.updated_at);
}

/**
 * Every decision waiting on the human, most urgent first and, at the same urgency, the one that
 * has waited longest first.
 */
export function decisionsOf(board: Board, details: Map<string, IssueDetail>, now: number): Decision[] {
  const rows = board.repos.flatMap((repo) => repo.rows.map((row) => ({ repo: repo.name, row })));
  const rowOf = (key: string | null) => (key ? rows.find((r) => r.row.issue_key === key) : undefined);
  const known = new Map<string, IssueRecord>();
  for (const { row } of rows) if (row.issue) known.set(row.issue.key, row.issue);
  for (const i of board.needs_me_issues) known.set(i.key, i);
  const issueOf = (key: string | null) => (key ? details.get(key)?.issue ?? known.get(key) ?? null : null);
  const out: Decision[] = [];

  for (const ask of board.asks) {
    const at = rowOf(ask.issue_key);
    const issue = issueOf(ask.issue_key);
    out.push({ id: `ask-${ask.id}`, kind: "ask", issue, row: at?.row ?? null, repo: at?.repo ?? null, ask, since: ask.asked_at, urgency: urgencyOf(issue) });
  }
  // The board lists an issue that waits on a person under its worktree row when it has one
  // (that row's needs_me is set), and in needs_me_issues only when it has none.
  // An issue whose review findings wait on the person (a request to accept one, and every one when
  // the review rounds stopped converging) is decided there, finding by finding: it is no approval
  // and no work of theirs to mark done, since its change failed review (NIT-220).
  const deciding = new Set(board.asks.filter((a) => a.finding && a.issue_key).map((a) => a.issue_key!.toUpperCase()));
  const approvals = new Map<string, IssueRecord>();
  for (const { row } of rows) if (row.issue && (needsHuman(row.issue) || isOwnWork(row.issue))) approvals.set(row.issue.key, row.issue);
  for (const i of board.needs_me_issues) if (!approvals.has(i.key)) approvals.set(i.key, i);
  for (const listed of approvals.values()) {
    if (deciding.has(listed.key.toUpperCase())) continue;
    // The board can be older than the issue's detail (a snapshot started before the human moved
    // it): an issue the detail shows approved, or moved anywhere else, is no longer a decision.
    const i = details.get(listed.key)?.issue ?? listed;
    // A playbook change is the human's own issue but a decision to approve, not work to do.
    const own = issueDecision(i, details.get(i.key)) === "own";
    if (!own && !needsHuman(i) && !isOwnWork(i)) continue;
    const at = rowOf(i.key);
    out.push({ id: `${own ? "own" : "approve"}-${i.key}`, kind: own ? "own" : "approve", issue: i, row: at?.row ?? null, repo: at?.repo ?? null, ask: null, since: waitingSince(i, details.get(i.key)), urgency: urgencyOf(i) });
  }
  for (const { repo, row } of rows) {
    if (!row.needs_me) continue;
    const conflict = !!row.state?.rebase_in_progress;
    // A row flagged only because its issue waits for approval is the approval above.
    if (!conflict && row.session?.state !== "waiting") continue;
    const issue = row.issue ?? issueOf(row.issue_key);
    // A conflict waits from when its rebase stopped; a branch's last commit can be days older.
    const since = conflict ? (row.state?.rebase_started_at ?? row.state?.last_commit_at ?? row.session?.last_seen_at ?? now) : (row.session?.last_seen_at ?? now);
    out.push({ id: `${conflict ? "conflict" : "input"}-${row.worktree}`, kind: conflict ? "conflict" : "input", issue, row, repo, ask: null, since, urgency: urgencyOf(issue) });
  }
  // A land that stopped, with no agent working on it any more, is the human's to unstick.
  for (const { repo, row } of rows) {
    if (!row.issue || row.state?.rebase_in_progress || row.session?.state === "waiting") continue;
    // The backend sends each issue's track; the preview's issues need their details for it.
    const detail = details.get(row.issue.key);
    const track = detail ? trackOf(detail.issue, detail.timeline, detail.reviews) : row.issue.track;
    if (!track || track.station !== "landing" || track.stuck !== "Land stopped") continue;
    if (livenessOf(row.session, now) === "working") continue;
    const stopped = detail?.timeline.filter((e) => e.kind === "land_failed").pop();
    out.push({ id: `land-${row.issue.key}`, kind: "land", issue: detail?.issue ?? row.issue, row, repo, ask: null, since: stopped?.ts ?? now, urgency: urgencyOf(detail?.issue ?? row.issue) });
  }

  return out.sort(byUrgency);
}

/** The id of the Inbox's card that holds several change approvals at once. */
export const APPROVALS = "approvals";

/**
 * The decisions as the Inbox lists them: when several changes wait for the person's approval at
 * once, they are one card ("3 changes wait for your approval") holding each of them, to approve or
 * send back one by one or all at once (docs/batching.md). Only plain change approvals group: a
 * playbook change, a goal plan or a mirrored issue is decided on its own card. A change just acted
 * on (`held`) stays in the card until the Inbox moves on from it.
 */
export function groupApprovals(decisions: Decision[], details: Map<string, IssueDetail>, held: Set<string> = new Set()): Decision[] {
  const plain = (d: Decision) => {
    if (d.kind !== "approve" || d.issue?.origin !== "nitely") return false;
    if (held.has(d.id)) return true;
    // Without its details it is not known yet whether it is a playbook change or a goal plan.
    const detail = details.get(d.issue.key);
    return !!detail && issueDecision(detail.issue, detail) === "approve";
  };
  const members = decisions.filter(plain);
  if (members.length < 2) return decisions;
  const group: Decision = {
    id: APPROVALS,
    kind: "approve",
    issue: null,
    row: null,
    repo: null,
    ask: null,
    since: Math.min(...members.map((d) => d.since)),
    urgency: Math.min(...members.map((d) => d.urgency)),
    group: [...members].sort(byUrgency),
  };
  return [...decisions.filter((d) => !members.includes(d)), group].sort(byUrgency);
}

/** Most urgent first and, at the same urgency, the one that has waited longest first. */
const byUrgency = (a: Decision, b: Decision) => a.urgency - b.urgency || a.since - b.since;

/**
 * What the human did to a decision: approved it, sent it back to Todo, answered it, marked their
 * own work done, or handed it back to agents; for a playbook change, applied or rejected it.
 */
export type Move = "approve" | "todo" | "answer" | "done" | "handback" | "apply" | "reject";

/** A decision the human just acted on, held in place until the Inbox moves on from it. */
export interface Resolved {
  decision: Decision;
  move: Move;
  /** The words of an answer, why a playbook change was rejected, or what applying it does. */
  answer?: string;
  /** False while the call is still on its way; it failing puts the decision back. */
  done: boolean;
}

/**
 * The decisions the Inbox lists: the live ones without those it has moved on from (`gone`; the
 * board can still name them for a moment), with each one just acted on held where it was, even
 * once the board has dropped it.
 */
export function shownDecisions(live: Decision[], resolved: Map<string, Resolved>, gone: Set<string>): Decision[] {
  const out = live.filter((d) => !gone.has(d.id) && !resolved.has(d.id));
  for (const r of resolved.values()) out.push(r.decision);
  return out.sort(byUrgency);
}

/** A question's first line with words in it: what a list row shows of it. */
export function firstLine(question: string): string {
  return question.split(/\r?\n/).map((l) => l.trim()).find((l) => l.length > 0) ?? "";
}

/**
 * For questions that share an issue, each one's place among them (oldest first) and how many
 * there are, so rows that would otherwise read alike say "question 2 of 3". A question alone on its
 * issue is not listed.
 */
export function askPlaces(decisions: Decision[]): Map<string, { n: number; of: number }> {
  const byIssue = new Map<string, Decision[]>();
  for (const d of decisions) {
    const key = d.kind === "ask" ? d.ask?.issue_key?.toUpperCase() : undefined;
    if (key) byIssue.set(key, [...(byIssue.get(key) ?? []), d]);
  }
  const out = new Map<string, { n: number; of: number }>();
  for (const asks of byIssue.values()) {
    if (asks.length < 2) continue;
    [...asks].sort((a, b) => a.ask!.asked_at - b.ask!.asked_at || a.ask!.id - b.ask!.id).forEach((d, i) => out.set(d.id, { n: i + 1, of: asks.length }));
  }
  return out;
}

/** What the card says once the human acted: short, in the past tense, and what comes next. */
export function outcomeOf(r: Pick<Resolved, "move" | "answer" | "decision">): string {
  switch (r.move) {
    case "approve":
      return "Approved. Landing next.";
    case "todo":
      return "Sent back to Todo.";
    case "answer":
      return `Answered${r.answer ? `: “${r.answer}”` : ""}. Sent to ${VENDOR_LABEL[vendorOf(r.decision.ask?.asked_by ?? null)]}.`;
    case "done":
      return "Marked done.";
    case "handback":
      return "Handed back to agents. In Todo for the next one.";
    case "apply":
      return `Approved.${r.answer ? ` ${r.answer}` : ""}`;
    case "reject":
      return `Rejected${r.answer ? `: “${r.answer}”` : ""}.`;
  }
}

/** Where the issue goes on the track after the move, shown before the backend says so. */
export function trackAfter(track: Track, move: Move, now: number): Track {
  switch (move) {
    case "approve":
      return { ...track, station: "landing", stuck: null, asking: false, reached: { ...track.reached, landing: now }, since: now };
    case "todo":
    case "handback":
      return { ...track, station: "todo", stuck: null, asking: false, since: now };
    case "done":
      return { ...track, station: "done", stuck: null, asking: false, reached: { ...track.reached, done: now }, since: now };
    case "answer":
    case "apply":
    case "reject":
      // Applying ends at Done or, for a committed playbook, back at Todo: the backend says which.
      return { ...track, asking: false, stuck: null };
  }
}
