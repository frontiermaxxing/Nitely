// How a goal's issues are drawn on its row: in station order, done to the left, and as a meter of
// whose move it is. Pure, so node:test runs it (src/goals.test.ts).

import type { IssueRecord } from "./types";
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
