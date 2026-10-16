// What waits on the human, as decisions: built from the board and, where the board alone cannot
// tell (why an approval waits, whether a land stopped), from issue details. The Inbox draws them.

import type { Ask, Board, IssueDetail, IssueRecord, TaskRow } from "./types";
// Straight from the modules rather than ./ui, so node:test can load this file without the CSS.
import { VENDOR_LABEL, livenessOf, vendorOf } from "./ui/agents.ts";
import { trackOf, type Track } from "./ui/lifecycle.ts";

export type DecisionKind = "ask" | "approve" | "conflict" | "input" | "land";

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
}

export const KIND_LABEL: Record<DecisionKind, string> = {
  ask: "question",
  approve: "approval",
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

/** The states that wait on a person (nitely-core's NEEDS_HUMAN). */
const NEEDS_HUMAN = ["Needs You", "Needs Andrew"];
export const needsHuman = (i: IssueRecord) => NEEDS_HUMAN.includes(i.state);

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
  const moved = detail?.timeline.filter((e) => e.kind === "issue_moved" && NEEDS_HUMAN.some((n) => e.summary?.includes(`→ ${n}`))).pop();
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
  const approvals = new Map<string, IssueRecord>();
  for (const { row } of rows) if (row.issue && needsHuman(row.issue)) approvals.set(row.issue.key, row.issue);
  for (const i of board.needs_me_issues) if (!approvals.has(i.key)) approvals.set(i.key, i);
  for (const listed of approvals.values()) {
    // The board can be older than the issue's detail (a snapshot started before the human moved
    // it): an issue the detail shows approved, or moved anywhere else, is no longer a decision.
    const i = details.get(listed.key)?.issue ?? listed;
    if (!needsHuman(i)) continue;
    const at = rowOf(i.key);
    out.push({ id: `approve-${i.key}`, kind: "approve", issue: i, row: at?.row ?? null, repo: at?.repo ?? null, ask: null, since: waitingSince(i, details.get(i.key)), urgency: urgencyOf(i) });
  }
  for (const { repo, row } of rows) {
    if (!row.needs_me) continue;
    const conflict = !!row.state?.rebase_in_progress;
    // A row flagged only because its issue waits for approval is the approval above.
    if (!conflict && row.session?.state !== "waiting") continue;
    const issue = row.issue ?? issueOf(row.issue_key);
    const since = conflict ? row.state?.last_commit_at ?? row.session?.last_seen_at ?? now : row.session?.last_seen_at ?? now;
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

/** Most urgent first and, at the same urgency, the one that has waited longest first. */
const byUrgency = (a: Decision, b: Decision) => a.urgency - b.urgency || a.since - b.since;

/** What the human did to a decision: approved it, sent it back to Todo, or answered it. */
export type Move = "approve" | "todo" | "answer";

/** A decision the human just acted on, held in place until the Inbox moves on from it. */
export interface Resolved {
  decision: Decision;
  move: Move;
  /** The words of an answer. */
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

/** What the card says once the human acted: short, in the past tense, and what comes next. */
export function outcomeOf(r: Pick<Resolved, "move" | "answer" | "decision">): string {
  switch (r.move) {
    case "approve":
      return "Approved. Landing next.";
    case "todo":
      return "Sent back to Todo.";
    case "answer":
      return `Answered${r.answer ? `: “${r.answer}”` : ""}. Sent to ${VENDOR_LABEL[vendorOf(r.decision.ask?.asked_by ?? null)]}.`;
  }
}

/** Where the issue goes on the track after the move, shown before the backend says so. */
export function trackAfter(track: Track, move: Move, now: number): Track {
  switch (move) {
    case "approve":
      return { ...track, station: "landing", stuck: null, asking: false, reached: { ...track.reached, landing: now }, since: now };
    case "todo":
      return { ...track, station: "todo", stuck: null, asking: false, since: now };
    case "answer":
      return { ...track, asking: false };
  }
}
