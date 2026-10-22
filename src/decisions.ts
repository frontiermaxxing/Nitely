// What waits on the human, as decisions: built from the board and, where the board alone cannot
// tell (why an approval waits, whether a land stopped), from issue details. The Inbox draws them.

import type { Ask, Board, IssueDetail, IssueRecord, Session, TaskRow } from "./types";
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
  /** The agent session it is about, when not the row's: a waiting agent, wherever it runs. */
  session?: Session | null;
  /** Where that agent works, when no task row says (a main checkout). */
  folder?: string | null;
  repo: string | null;
  ask: Ask | null;
  /** When it started waiting on the human. */
  since: number;
  /** 1 (urgent) to 4 (low); 5 when the issue has no priority or there is no issue. */
  urgency: number;
  /** The Inbox's card for several decisions at once: change approvals (`groupApprovals`), or
   *  one issue's review findings (`groupFindings`): each one. */
  group?: Decision[];
  /** The project it belongs to (`projectOf`), for the Inbox's project filter. */
  project?: string | null;
}

/** The agent session a decision is about: its own (a waiting agent), else its row's. */
export function sessionOf(d: Decision): Session | null {
  return d.session ?? d.row?.session ?? null;
}

/** The folder a decision's agent works in: its row's worktree, else the agent's own folder. */
export function folderOf(d: Decision): string | null {
  return d.row?.worktree ?? d.folder ?? null;
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

/**
 * What the board says of each issue whose details the Inbox reads (`detailKeys`), as one string per
 * key: its record (state, track, owner, updated), its questions, its worktree's state and its pull
 * request (checks). A key
 * whose string is unchanged since the last board needs no new read of its details (NIT-337).
 */
export function detailFingerprints(board: Board): Map<string, string> {
  const rows = board.repos.flatMap((r) => r.rows);
  const out = new Map<string, string>();
  for (const key of detailKeys(board)) {
    const row = rows.find((r) => r.issue_key === key || r.issue?.key === key);
    const issue = row?.issue ?? board.needs_me_issues.find((i) => i.key === key) ?? null;
    const asks = board.asks.filter((a) => a.issue_key === key).map((a) => [a.id, a.answer, a.finding?.text ?? null]);
    const state = row?.state ? [row.state.rebase_in_progress, row.state.ahead, row.state.behind, row.state.last_commit_at] : null;
    // Its pull request too: a CI result that changes is what an approval's card shows.
    out.set(key, JSON.stringify([issue, asks, state, row?.needs_me ?? null, row?.session?.state ?? null, row?.pr ?? null]));
  }
  return out;
}

/**
 * The keys whose details to read again: new ones, ones whose fingerprint changed, ones not read
 * yet, and every one when `all` (the hook does that now and then, so nothing stays stale forever).
 */
export function detailsToRead(now: Map<string, string>, before: Map<string, string>, have: Set<string>, all: boolean): string[] {
  return [...now.keys()].filter((k) => all || !have.has(k) || before.get(k) !== now.get(k));
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
  // A plan the review mode started is shown by its card too, as a notice with Cancel (NIT-550).
  if (detail?.goal_plan?.status === "pending" || detail?.goal_plan?.status === "started") return "goal-plan";
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
  // Each decision's project: its issue's, else its repo's, else its key's prefix.
  const repoProject = new Map(board.repos.map((r) => [r.name, r.project]));
  const withProject = (d: Decision): Decision => ({ ...d, project: d.issue?.project ?? (d.repo ? repoProject.get(d.repo) : null) ?? projectOf(d) });

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
    if (!row.needs_me || !row.state?.rebase_in_progress) continue;
    const issue = row.issue ?? issueOf(row.issue_key);
    // A conflict waits from when its rebase stopped; a branch's last commit can be days older.
    const since = row.state?.rebase_started_at ?? row.state?.last_commit_at ?? row.session?.last_seen_at ?? now;
    out.push({ id: `conflict-${row.worktree}`, kind: "conflict", issue, row, repo, ask: null, since, urgency: urgencyOf(issue) });
  }
  // An agent waiting at its own prompt, one per session wherever it runs (NIT-273). One waiting on
  // a question it asked is that question, above.
  for (const a of board.agents ?? []) {
    if (a.session.state !== "waiting" || board.asks.some((q) => q.asked_by === a.session.id)) continue;
    const row = a.worktree ? (rows.find((r) => r.row.worktree === a.worktree)?.row ?? null) : null;
    const issue = a.issue ?? issueOf(a.issue_key);
    out.push({ id: `input-${a.session.id}`, kind: "input", issue, row, session: a.session, folder: a.worktree ?? a.session.cwd, repo: a.repo, ask: null, since: a.session.last_seen_at, urgency: urgencyOf(issue) });
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

  return out.map(withProject).sort(byUrgency);
}

/** "NIT" for "NIT-12"; null for anything that is not an issue key. */
export function keyProject(key: string | null | undefined): string | null {
  return key?.match(/^([A-Za-z][A-Za-z0-9]*)-\d+$/)?.[1].toUpperCase() ?? null;
}

/** The project a decision belongs to, for the Inbox's filter; null when nothing says (a session outside any project). */
export function projectOf(d: Decision): string | null {
  if (d.project !== undefined && d.project !== null) return d.project;
  if (d.group?.length) {
    const all = new Set(d.group.map(projectOf));
    return all.size === 1 ? [...all][0] : null;
  }
  return d.issue?.project ?? keyProject(d.issue?.key ?? d.ask?.issue_key ?? d.row?.issue_key);
}

/** A project with decisions waiting, and how many. */
export interface ProjectCount {
  project: string;
  count: number;
}

/**
 * The Inbox's project chips: each project with decisions waiting and how many, most first, then by
 * key. Each decision counts once, so the counts add up to the Inbox's (a decision without a
 * project counts only under All).
 */
export function projectCounts(decisions: Decision[]): ProjectCount[] {
  const counts = new Map<string, number>();
  for (const d of decisions) {
    const p = projectOf(d);
    if (p) counts.set(p, (counts.get(p) ?? 0) + 1);
  }
  return [...counts].map(([project, count]) => ({ project, count })).sort((a, b) => b.count - a.count || a.project.localeCompare(b.project));
}

/** The decisions of `project`, or all of them for null. */
export function inProject(decisions: Decision[], project: string | null): Decision[] {
  return project ? decisions.filter((d) => projectOf(d)?.toUpperCase() === project.toUpperCase()) : decisions;
}

/** The project after (`by` 1) or before (-1) `current` in the chips' order, All (null) between the last and the first. */
export function nextProject(counts: ProjectCount[], current: string | null, by: 1 | -1): string | null {
  const order: (string | null)[] = [null, ...counts.map((c) => c.project)];
  const at = Math.max(0, order.findIndex((p) => (p ?? "").toUpperCase() === (current ?? "").toUpperCase()));
  return order[(at + by + order.length) % order.length];
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

/** The id of the Inbox's card for one issue's review findings to decide: this, then its key. */
export const FINDINGS = "findings-";

/**
 * One question per issue for its review findings (NIT-571): when several requests to accept a
 * finding of one issue wait (the review rounds stalled, or the agent asked about more than one),
 * they are one card listing each finding, each still accepted or sent back on its own, and a fix
 * still closes its own. One alone is its own card, as before.
 */
export function groupFindings(decisions: Decision[]): Decision[] {
  const byIssue = new Map<string, Decision[]>();
  for (const d of decisions) {
    const key = d.kind === "ask" && d.ask?.finding ? d.ask.issue_key?.toUpperCase() : undefined;
    if (key) byIssue.set(key, [...(byIssue.get(key) ?? []), d]);
  }
  let out = decisions;
  for (const [key, members] of byIssue) {
    if (members.length < 2) continue;
    const first = members[0];
    const group: Decision = {
      id: `${FINDINGS}${key}`,
      kind: "ask",
      issue: first.issue,
      row: first.row,
      repo: first.repo,
      ask: null,
      since: Math.min(...members.map((d) => d.since)),
      urgency: Math.min(...members.map((d) => d.urgency)),
      group: [...members].sort((a, b) => a.ask!.id - b.ask!.id),
      project: first.project,
    };
    out = [...out.filter((d) => !members.includes(d)), group];
  }
  return out === decisions ? decisions : out.sort(byUrgency);
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
