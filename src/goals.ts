// How a goal's issues are drawn on its row: in station order, done to the left, and as a meter of
// whose move it is. Pure, so node:test runs it (src/goals.test.ts).

import type { Goal, IssueRecord } from "./types";
// Straight from the module rather than ./ui, so node:test can load this file without the CSS.
import { stationIndex, trackOf, type Track } from "./ui/lifecycle.ts";

export interface Placed {
  issue: IssueRecord;
  track: Track;
}

/** Where an issue sits on its goal's row: done first, then the furthest along, Backlog last. */
function rank(t: Track): number {
  if (t.station === "done") return 0;
  if (t.backlog) return 99;
  return 8 - stationIndex(t.station);
}

const number = (key: string) => Number(key.split("-").pop()) || 0;

/** A goal's issues in row order; canceled and duplicate ones are no longer part of it. */
export function goalOrder(issues: IssueRecord[]): Placed[] {
  return issues
    .map((issue) => ({ issue, track: trackOf(issue) }))
    .filter((p) => !p.track.closed)
    .sort((a, b) => rank(a.track) - rank(b.track) || number(a.issue.key) - number(b.issue.key));
}

/** How a goal's issues stand, by the one colour rule: done, an agent moving it, waiting on you, failed, queued. */
export interface GoalCounts {
  done: number;
  moving: number;
  needs: number;
  stuck: number;
  queued: number;
}

export function goalCounts(placed: Placed[]): GoalCounts {
  const c: GoalCounts = { done: 0, moving: 0, needs: 0, stuck: 0, queued: 0 };
  for (const { track: t } of placed) {
    if (t.station === "done") c.done++;
    else if (t.stuck) c.stuck++;
    // An agent's open question waits on you too, whatever state the issue is in.
    else if (t.station === "needs" || t.asking) c.needs++;
    else if (t.station === "todo" || t.backlog) c.queued++;
    else c.moving++;
  }
  return c;
}

/** "2 working · 3 landed today": the crew on a goal now and its last day's landings, or null when neither. */
export function crewOnGoal(goal: { working?: number; landed_today?: number }): string | null {
  const parts = [goal.working ? `${goal.working} working` : null, goal.landed_today ? `${goal.landed_today} landed today` : null].filter(Boolean);
  return parts.length ? parts.join(" · ") : null;
}

/**
 * The goal branch on the goal row: "goal-19 · 4 items · next checkpoint at 10 items (6 to go)", red
 * with what it waits for, or null for a goal whose items land on main.
 */
export function goalBranchLine(branch: { branch: string; created: boolean; state: string; red_reason?: string | null; red_issue?: string | null; items: number; next_checkpoint: string } | undefined): { text: string; red: boolean } | null {
  if (!branch) return null;
  if (!branch.created) return { text: `${branch.branch} · made when its first item starts`, red: false };
  if (branch.state === "red") {
    const fix = branch.red_issue ? `; ${branch.red_issue} fixes it` : "";
    return { text: `${branch.branch} red: ${branch.red_reason ?? "it cannot take main"}${fix}`, red: true };
  }
  const items = `${branch.items} item${branch.items === 1 ? "" : "s"} since the last checkpoint`;
  // "at 10 items" and "due now" read after "next checkpoint"; "NIT-612 is landing it" alone.
  const next = /^(at|due)\b/.test(branch.next_checkpoint) ? `next checkpoint ${branch.next_checkpoint}` : `checkpoint: ${branch.next_checkpoint}`;
  return { text: `${branch.branch} · ${items} · ${next}`, red: false };
}

/** A risk's colour by the one colour rule: stuck is failed, waiting on you is attention, many rounds is neither. */
export function riskTone(why: string): "fail" | "attention" | "plain" {
  if (why.startsWith("stuck")) return "fail";
  if (why === "waits on you" || why === "asked you something") return "attention";
  return "plain";
}

/**
 * "You were needed 3 times: 2 questions, 1 approval · 1 still waits", or null when never; "… 9
 * times on 4 issues: …" when some issue needed you more than once.
 */
export function neededLine(needed: { kind: "asked" | "approval"; open: boolean; issue_key?: string | null }[]): string | null {
  if (needed.length === 0) return null;
  const asked = needed.filter((n) => n.kind === "asked").length;
  const approvals = needed.length - asked;
  const parts = [asked ? `${asked} question${asked === 1 ? "" : "s"}` : null, approvals ? `${approvals} approval${approvals === 1 ? "" : "s"}` : null].filter(Boolean);
  const open = needed.filter((n) => n.open).length;
  const times = needed.length === 1 ? "once" : `${needed.length} times`;
  const issues = groupNeeded(needed.map((n, i) => ({ ts: i, summary: "", issue_key: n.issue_key ?? null, ...n }))).length;
  const on = issues < needed.length ? ` on ${issues} issue${issues === 1 ? "" : "s"}` : "";
  return `You were needed ${times}${on}: ${parts.join(", ")}${open ? ` · ${open === needed.length && open > 1 ? "all" : open} still wait${open === 1 ? "s" : ""}` : ""}`;
}

/** The places one issue needed you, as one row of the report. */
export interface NeededGroup {
  issue_key: string | null;
  /** The newest of them, which the row shows. */
  ts: number;
  summary: string;
  asked: number;
  approvals: number;
  /** How many still wait on you. */
  open: number;
}

/**
 * The report's needed entries, one row per issue (NIT-355): an issue whose review stopped
 * converging asks once per finding, and a night of those reads as a column of the same line.
 * What still waits comes first, then the newest. An entry with no issue stands alone.
 */
export function groupNeeded(needed: { ts: number; kind: "asked" | "approval"; issue_key: string | null; summary: string; open: boolean }[]): NeededGroup[] {
  const groups = new Map<string, NeededGroup>();
  needed.forEach((n, i) => {
    const id = n.issue_key ? n.issue_key.toUpperCase() : `#${i}`;
    const g = groups.get(id) ?? { issue_key: n.issue_key, ts: 0, summary: "", asked: 0, approvals: 0, open: 0 };
    if (n.kind === "asked") g.asked++;
    else g.approvals++;
    if (n.open) g.open++;
    // The row says what the newest one was about; one still waiting wins over an older settled one.
    if (n.ts >= g.ts) {
      g.ts = n.ts;
      g.summary = n.summary;
    }
    groups.set(id, g);
  });
  return [...groups.values()].sort((a, b) => Number(b.open > 0) - Number(a.open > 0) || b.ts - a.ts);
}

/** "asked", "approval", or "3 questions, 1 approval" for a row that stands for several. */
export function groupKind(g: NeededGroup): string {
  if (g.asked + g.approvals === 1) return g.asked ? "asked" : "approval";
  const part = (n: number, one: string) => (n ? `${n} ${one}${n === 1 ? "" : "s"}` : null);
  return [part(g.asked, "question"), part(g.approvals, "approval")].filter(Boolean).join(", ");
}

/** Landed issues counted by project, the most first: "NIT 40 · AND 26 · GR 9". */
export function byProject(landed: { key: string }[]): { project: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const l of landed) {
    const project = l.key.includes("-") ? l.key.slice(0, l.key.lastIndexOf("-")) : l.key;
    counts.set(project, (counts.get(project) ?? 0) + 1);
  }
  return [...counts.entries()].map(([project, count]) => ({ project, count })).sort((a, b) => b.count - a.count || a.project.localeCompare(b.project));
}

/** "Triage answered 4 questions without you", or null when none. */
export function triagedLine(count: number): string | null {
  if (count === 0) return null;
  return `Triage answered ${count === 1 ? "a question" : `${count} questions`} without you`;
}

const shortDate = (d: Date, today: Date) =>
  d.toLocaleDateString("en-US", { month: "short", day: "numeric", ...(d.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}) });

/**
 * "done ~Oct 22 at this pace", from the goal's landings over the last three days; `late` when that
 * is after its target date. Null without a pace (nothing landed lately) or with nothing left.
 */
export function finishLine(finish: { at: number; per_day: number } | null | undefined, target: string | null | undefined, today: Date): { text: string; late: boolean; title: string } | null {
  if (!finish) return null;
  const at = new Date(finish.at * 1000);
  const start = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  // Rounded: across a daylight-saving change a calendar day is 23 or 25 hours.
  const days = Math.round((new Date(at.getFullYear(), at.getMonth(), at.getDate()).getTime() - start.getTime()) / 86400000);
  const label = days <= 0 ? "today" : days === 1 ? "tomorrow" : `~${shortDate(at, today)}`;
  let late = false;
  if (target) {
    const [y, m, d] = target.split("-").map(Number);
    late = at.getTime() >= new Date(y, m - 1, d + 1).getTime();
  }
  const rate = finish.per_day >= 1 ? `${Math.round(finish.per_day * 10) / 10} a day` : `one every ${Math.round(1 / finish.per_day)} days`;
  return { text: `done ${label} at this pace`, late, title: `Its issues landed ${rate} over the last three days${late ? ", which finishes after its target date" : ""}` };
}

/** "by Dec 1 · in 12 days", "by Nov 2 · 3 days late", or null without a date. Late only while open. */
export function targetLine(date: string | null | undefined, today: Date, complete: boolean): { text: string; late: boolean } | null {
  if (!date) return null;
  const [y, m, d] = date.split("-").map(Number);
  const target = new Date(y, m - 1, d);
  const start = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const days = Math.round((target.getTime() - start.getTime()) / 86400000);
  const label = target.toLocaleDateString("en-US", { month: "short", day: "numeric", ...(y !== today.getFullYear() ? { year: "numeric" } : {}) });
  if (complete) return { text: `by ${label}`, late: false };
  if (days < 0) return { text: `by ${label} · ${-days} day${days === -1 ? "" : "s"} late`, late: true };
  if (days === 0) return { text: `by ${label} · today`, late: false };
  return { text: `by ${label} · in ${days} day${days === 1 ? "" : "s"}`, late: false };
}

/** Finished cards are hidden without declaring the goal's outcome achieved. Pending checkpoints stay visible. */
export function goalWorkFinished(goal: Goal): boolean {
  const b = goal.branch;
  return goal.progress.total > 0 && goal.progress.open === 0 &&
    !(b && (b.state === "red" || b.items > 0 || b.checkpoint_issue));
}

export function showGoal(goal: Goal, completed: boolean, focused?: number): boolean {
  return completed || goal.id === focused || (goal.state === "active" && !goalWorkFinished(goal));
}
