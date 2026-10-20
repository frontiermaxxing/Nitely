// What the app says about build lanes (docs/build-lanes.md, "What the app shows"): one chip per
// lane on Now and Overview, and each run on its issue's timeline. Pure, so its tests run alone.

import type { LaneRun, LanesOverview, MachineNow, ProjectLanes } from "./types";

export function executorName(executor = "local"): string {
  return executor === "ci" ? "CI" : executor.startsWith("machine:") ? `Machine ${executor.slice(8)}` : "This machine";
}

const remote = (r: LaneRun) => r.executor === "ci" || r.executor.startsWith("machine:");
const active = (r: LaneRun) => r.state === "running" || r.state === "queued";

/** Every active remote request, then the three newest completed outcomes; local slots stay separate. */
export function remoteRuns(p: ProjectLanes): LaneRun[] {
  const seen = new Set<number>();
  const rows = [...p.queue, ...p.recent].filter((r) => {
    if (!remote(r) || seen.has(r.id)) return false;
    seen.add(r.id);
    return true;
  });
  const running = rows.filter(active).sort((a, b) => a.queued_at - b.queued_at);
  const recent = rows.filter((r) => !active(r)).sort((a, b) => (b.ended_at ?? b.queued_at) - (a.ended_at ?? a.queued_at)).slice(0, 3);
  return [...running, ...recent];
}

export function remoteWords(r: LaneRun, now: number): { state: "running" | "idle" | "unhealthy" | "unknown"; label: string; duration: string; detail: string; checks: string | null } {
  let state: "running" | "idle" | "unhealthy" | "unknown" = "unknown";
  let label = "Outcome unknown";
  if (active(r)) { state = "running"; label = "In progress"; }
  else if (r.state === "done" && r.exit_code === 0 && !r.timed_out) { state = "idle"; label = "Passed"; }
  else if (r.timed_out) { state = "unhealthy"; label = "Timed out"; }
  else if (r.state === "done" && r.exit_code != null) { state = "unhealthy"; label = `Failed (exit ${r.exit_code})`; }
  else if (r.state === "lane_error") { state = "unhealthy"; label = "Executor error"; }
  let detail = r.error ?? (active(r) ? "Elapsed time includes queueing on the other machine. Remote resource usage is not measured here."
    : state === "unknown" ? "The connection ended before a result was recorded. Work may still be running remotely." : "");
  let checks: string | null = null;
  // Output is bounded and may be truncated; never render raw protocol JSON or guess a result URL.
  try {
    const parsed: unknown = JSON.parse(r.output);
    if (parsed && typeof parsed === "object" && "results" in parsed && Array.isArray(parsed.results)) {
      const outputs: string[] = [];
      for (const result of parsed.results) {
        if (!result || typeof result !== "object" || typeof result.output !== "string") continue;
        if (r.executor === "ci" && result.command === "CI required checks"
          && /^https:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+\/commit\/[a-fA-F0-9]{40,64}\/checks$/.test(result.output)) checks = result.output;
        else if (result.output) outputs.push(result.output);
      }
      if (!detail) detail = outputs.join("\n\n");
    }
  } catch { /* An incomplete result remains incomplete; the recorded state is still available. */ }
  const end = r.ended_at ?? (active(r) ? now : r.started_at ?? r.queued_at);
  return { state, label, duration: forHowLong(end - (r.started_at ?? r.queued_at)), detail, checks };
}

/** One lane as its chip shows it. */
export interface LaneChip {
  project: string;
  /** Its number; 0 is the quick lane, only for quick runs (NIT-347). */
  lane: number;
  /** running a request; idle at a commit; not made yet (made on its first request); unhealthy:
   * its last request ended in a lane problem, not a test failure; or waiting: the person's main
   * checkout (large-repo mode) has changes or is on another branch, so a request waits for it. */
  state: "running" | "idle" | "unmade" | "unhealthy" | "waiting";
  /** Large-repo mode: the lane is the person's own main checkout. */
  checkout: boolean;
  /** Why a request waits for the main checkout. */
  waitingFor: string | null;
  /** The request it runs now. */
  run: LaneRun | null;
  /** The commit it holds. */
  at: string | null;
  /** Why it is unhealthy, in one line. */
  problem: string | null;
}

/** The chips of a project's lanes, in lane order. A running request wins over an older problem.
 * The quick lane has a chip once it is made: most repos never need one. */
export function laneChips(p: ProjectLanes): LaneChip[] {
  const checkout = !!p.rules.large_repo;
  return p.lanes.filter((l) => l.made || !isQuick(l)).map((l) => {
    const run = p.queue.find((r) => r.state === "running" && r.lane === l.lane) ?? null;
    const last = (p.last ?? p.recent).find((r) => r.lane === l.lane) ?? null;
    const waitingFor = checkout ? (p.queue.find((r) => r.state === "queued" && r.waiting_for)?.waiting_for ?? null) : null;
    const base = { project: p.project, lane: l.lane, run, at: l.at, problem: null, checkout, waitingFor };
    if (run) return { ...base, state: "running" };
    if (waitingFor) return { ...base, state: "waiting" };
    if (last?.state === "lane_error") return { ...base, state: "unhealthy", problem: firstLine(last.error) ?? "the lane could not run its last request" };
    return { ...base, state: l.made ? "idle" : "unmade" };
  });
}

/** Requests waiting for a lane (not yet running). */
export function waiting(p: ProjectLanes): number {
  return p.queue.filter((r) => r.state === "queued").length;
}

/** How long a request may wait before the strip says so: past this, a requester may have stopped
 * coming for it (NIT-270), and `nitely lanes cancel` takes it out. */
export const LONG_WAIT_SECONDS = 10 * 60;

/** "3 waiting", or "3 waiting, longest 14m" once the oldest has waited long, and a line per request
 * for its tooltip: why it waits, and how to cancel one that has waited long. */
export function queueWords(p: ProjectLanes, now: number): { label: string; long: boolean; lines: string[] } {
  const queued = p.queue.filter((r) => r.state === "queued");
  const longest = Math.max(0, ...queued.map((r) => now - r.queued_at));
  const long = longest >= LONG_WAIT_SECONDS;
  const lines = queued.map((r) => {
    const waited = now - r.queued_at;
    const reason = r.waiting_why ?? r.waiting_memory;
    const why = reason ? ` (${reason})` : "";
    const cancel = waited >= LONG_WAIT_SECONDS ? `, waited ${forHowLong(waited)}: \`nitely lanes cancel ${r.id}\` takes it out` : "";
    return `${r.issue ?? "no issue"}: ${r.command}${why}${cancel}`;
  });
  return { label: `${queued.length} waiting${long ? `, longest ${forHowLong(longest)}` : ""}`, long, lines };
}

/** "8.3 GB", "640 MB". */
export function bytesWords(bytes: number): string {
  const gb = bytes / 1024 ** 3;
  return gb >= 1 ? `${gb.toFixed(1)} GB` : `${Math.round(bytes / 1024 ** 2)} MB`;
}

/** What a project's lanes may grow to: the playbook's cap, or twice each lane's first build. */
export function capWords(p: ProjectLanes): string {
  const gb = p.rules.size_cap_gb;
  return gb && gb > 0 ? `${gb} GB per lane` : "twice each lane's first build";
}

/** "lane 1: 8.3 GB of 12.8 GB", for each lane that has run a request. */
export function sizeWords(p: ProjectLanes): string[] {
  return p.lanes
    .filter((l) => l.size_bytes != null)
    .map((l) => `${isQuick(l) ? "quick lane" : `lane ${l.lane}`}: ${bytesWords(l.size_bytes!)}${l.cap_bytes ? ` of ${bytesWords(l.cap_bytes)}` : ""}`);
}

/** "42s", "3m", "1h 5m": how long something has run. */
export function forHowLong(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  const m = Math.floor((s % 3600) / 60);
  return m ? `${Math.floor(s / 3600)}h ${m}m` : `${Math.floor(s / 3600)}h`;
}

/** What the strip's heading says about the machine: "builds 2 of 3 · waiting for memory · below
 * normal priority" (NIT-266). */
export function machineWords(o: LanesOverview): string[] {
  const words: string[] = [];
  if (o.slots) words.push(`builds ${o.slots.used} of ${o.slots.total}`);
  if (o.waiting_for_memory) words.push("waiting for memory");
  if (o.machine?.below_normal) words.push("below normal priority");
  return words;
}

/** Settings' words for the machine's memory: free now, and each limit with where it comes from. */
export function memoryWords(m: MachineNow | undefined): { free: string | null; floor: string | null; cap: string | null } {
  const from = (f: string) => (f === "default" ? "default" : f === "settings" ? "set here" : `set by ${f}`);
  const l = m?.limits;
  return {
    free: m?.memory ? `${bytesWords(m.memory.available)} of ${bytesWords(m.memory.total)} free now` : null,
    floor: l ? (l.floor === 0 ? `off (${from(l.floor_from)})` : `${bytesWords(l.floor)} free (${from(l.floor_from)})`) : null,
    cap: l
      ? m?.run_cap != null && m.run_cap < l.cap
        ? `${bytesWords(m.run_cap)} per run (${bytesWords(l.cap)} ${from(l.cap_from)}, shared by the slots above the floor)`
        : `${bytesWords(l.cap)} per run (${from(l.cap_from)})`
      : null,
  };
}

/** The Settings form's floor and cap as the command takes them, GB or null for the default, or
 * what is wrong with them. */
export function limitsOf(floor: string, cap: string): { floorGb: number | null; capGb: number | null } | string {
  const gb = (v: string) => (v.trim() === "" ? null : Number(v));
  const floorGb = gb(floor);
  const capGb = gb(cap);
  if (floorGb !== null && (!Number.isFinite(floorGb) || floorGb < 0)) return "The floor is a number of GB, 0 or more (0 turns the wait off).";
  if (capGb !== null && (!Number.isFinite(capGb) || capGb < 0.25)) return "The cap is a number of GB, at least 0.25.";
  return { floorGb, capGb };
}

/** What a chip calls its lane after the project: its number, or the main checkout in large-repo mode. */
export function chipName(c: LaneChip): string {
  if (c.checkout) return `${c.project} main checkout`;
  return c.lane === 0 ? `${c.project} quick` : `${c.project} ${c.lane}`;
}

/** The quick lane (NIT-347), by its flag or, from an older nitely, its number. */
function isQuick(l: { lane: number; quick?: boolean }): boolean {
  return !!l.quick || l.lane === 0;
}

/** What a chip says after its project and lane. */
export function chipWords(c: LaneChip, now: number): string {
  switch (c.state) {
    case "running": {
      const r = c.run!;
      const reserved = r.memory_reserved ? `${bytesWords(r.memory_reserved)} set aside` : null;
      const words = [r.command, r.issue, forHowLong(now - (r.started_at ?? r.queued_at)), reserved].filter(Boolean).join(" · ");
      return c.checkout ? `building in your checkout: ${words}` : words;
    }
    case "waiting":
      return `waiting: ${c.waitingFor}`;
    case "unhealthy":
      return `problem: ${c.problem}`;
    case "unmade":
      return "made on its first run";
    case "idle":
      return c.at ? `idle at ${c.at}` : "idle";
  }
}

/** The run id a lane entry on an issue's timeline names: `Ran \`cargo test\` in lane 1: exit 0 after
 * 42 s (run 17)`, or, for a gate answered from an earlier pass, `passed in lane run 17 already`. */
export function laneRunId(t: { kind: string; summary: string | null }): number | null {
  if (t.kind !== "lane_run") return null;
  const m = t.summary?.match(/\(run (\d+)\)\s*$/) ?? t.summary?.match(/\blane run (\d+)\b/);
  return m ? Number(m[1]) : null;
}

function firstLine(text: string | null | undefined): string | null {
  const line = text?.split("\n").find((l) => l.trim())?.trim();
  return line || null;
}
