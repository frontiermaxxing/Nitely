// What the app says about build lanes (docs/build-lanes.md, "What the app shows"): one chip per
// lane on Now and Overview, and each run on its issue's timeline. Pure, so its tests run alone.

import type { LaneRun, ProjectLanes } from "./types";

/** One lane as its chip shows it. */
export interface LaneChip {
  project: string;
  lane: number;
  /** running a request; idle at a commit; not made yet (made on its first request); or unhealthy:
   * its last request ended in a lane problem, not a test failure. */
  state: "running" | "idle" | "unmade" | "unhealthy";
  /** The request it runs now. */
  run: LaneRun | null;
  /** The commit it holds. */
  at: string | null;
  /** Why it is unhealthy, in one line. */
  problem: string | null;
}

/** The chips of a project's lanes, in lane order. A running request wins over an older problem. */
export function laneChips(p: ProjectLanes): LaneChip[] {
  return p.lanes.map((l) => {
    const run = p.queue.find((r) => r.state === "running" && r.lane === l.lane) ?? null;
    const last = p.recent.find((r) => r.lane === l.lane) ?? null;
    const base = { project: p.project, lane: l.lane, run, at: l.at, problem: null };
    if (run) return { ...base, state: "running" };
    if (last?.state === "lane_error") return { ...base, state: "unhealthy", problem: firstLine(last.error) ?? "the lane could not run its last request" };
    return { ...base, state: l.made ? "idle" : "unmade" };
  });
}

/** Requests waiting for a lane (not yet running). */
export function waiting(p: ProjectLanes): number {
  return p.queue.filter((r) => r.state === "queued").length;
}

/** "42s", "3m", "1h 5m": how long something has run. */
export function forHowLong(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  const m = Math.floor((s % 3600) / 60);
  return m ? `${Math.floor(s / 3600)}h ${m}m` : `${Math.floor(s / 3600)}h`;
}

/** What a chip says after its project and lane. */
export function chipWords(c: LaneChip, now: number): string {
  switch (c.state) {
    case "running": {
      const r = c.run!;
      return [r.command, r.issue, forHowLong(now - (r.started_at ?? r.queued_at))].filter(Boolean).join(" · ");
    }
    case "unhealthy":
      return `problem: ${c.problem}`;
    case "unmade":
      return "made on its first run";
    case "idle":
      return c.at ? `idle at ${c.at}` : "idle";
  }
}

/** The run id a lane entry on an issue's timeline names: `Ran \`cargo test\` in lane 1: exit 0 after 42 s (run 17)`. */
export function laneRunId(t: { kind: string; summary: string | null }): number | null {
  if (t.kind !== "lane_run") return null;
  const m = t.summary?.match(/\(run (\d+)\)\s*$/);
  return m ? Number(m[1]) : null;
}

function firstLine(text: string | null | undefined): string | null {
  const line = text?.split("\n").find((l) => l.trim())?.trim();
  return line || null;
}
