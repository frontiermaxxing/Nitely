import type { IssueRecord, ReviewRecord, TimelineEntry } from "../types";

/** The fixed stations every issue moves through, in order. docs/visualization.md. */
export const STATIONS = ["todo", "started", "working", "gate", "review", "needs", "landing", "done"] as const;
export type Station = (typeof STATIONS)[number];

export const STATION_LABEL: Record<Station, string> = {
  todo: "Todo",
  started: "Started",
  working: "Working",
  gate: "Gate",
  review: "Review",
  needs: "Needs you",
  landing: "Landing",
  done: "Done",
};

/** Whose move it is at a station, which decides its color. */
export type Phase = "queued" | "agent" | "human" | "done";

export function phaseOf(station: Station): Phase {
  if (station === "todo") return "queued";
  if (station === "needs") return "human";
  if (station === "done") return "done";
  return "agent";
}

/** Where an issue is on the track, and how it got there. */
export interface Track {
  station: Station;
  /** When each station was first reached. Stations behind `station` that are missing were skipped. */
  reached: Partial<Record<Station, number>>;
  /** Review rounds that asked for changes (the "changes asked" loop back to Working). */
  rounds: number;
  /** Why the issue is stuck at its station, if it is. */
  stuck: string | null;
  /** An agent asked something that is not answered yet. */
  asking: boolean;
  /** Not on the track any more: canceled or a duplicate. */
  closed: "canceled" | "duplicate" | null;
  /** Still in Backlog: before Todo, drawn fainter. */
  backlog: boolean;
}

export function stationIndex(station: Station): number {
  return STATIONS.indexOf(station);
}

/** The station an issue state alone implies (Linear issues have no timeline to refine it). */
export function stationOfState(state: string, stateType = ""): Station {
  switch (state) {
    case "Backlog":
    case "Todo":
      return "todo";
    case "In Progress":
      return "working";
    case "In Review":
      return "review";
    case "Needs You":
    case "Needs Andrew":
      return "needs";
    case "Approved":
      return "landing";
    case "Done":
    case "Canceled":
    case "Duplicate":
      return "done";
  }
  if (stateType === "completed" || stateType === "canceled") return "done";
  if (stateType === "started") return "working";
  return "todo";
}

function closedOf(issue: IssueRecord): Track["closed"] {
  if (issue.state === "Duplicate") return "duplicate";
  if (issue.state === "Canceled" || (issue.state_type === "canceled" && issue.state !== "Duplicate")) return "canceled";
  return null;
}

/** "Todo → In Progress" → "In Progress"; "In Progress → Done: tests passed" → "Done". */
function movedTo(summary: string | null): string | null {
  const arrow = summary?.indexOf("→") ?? -1;
  if (!summary || arrow < 0) return null;
  const rest = summary.slice(arrow + 1);
  const colon = rest.indexOf(":");
  return (colon < 0 ? rest : rest.slice(0, colon)).trim() || null;
}

/**
 * The issue's track. The backend computes it (crates/nitely-core/src/track.rs) and sends it on
 * every issue, so rows, the panel and Pulse agree; this falls back to `computeTrack` only for
 * issues without one, such as the browser preview's sample data.
 */
export function trackOf(issue: IssueRecord, timeline?: TimelineEntry[], reviews?: ReviewRecord[]): Track {
  return issue.track ?? computeTrack(issue, timeline, reviews);
}

/**
 * The track from the issue's state and, when given, its timeline and review records: the same
 * rules as track.rs, for the preview's fixtures. Without a timeline only the station is known.
 */
export function computeTrack(issue: IssueRecord, timeline?: TimelineEntry[], reviews?: ReviewRecord[]): Track {
  const track: Track = {
    station: stationOfState(issue.state, issue.state_type),
    reached: {},
    rounds: 0,
    stuck: null,
    asking: false,
    closed: closedOf(issue),
    backlog: issue.state === "Backlog",
  };
  if (!timeline?.length) {
    track.rounds = reviews?.filter((r) => r.verdict === "changes").length ?? 0;
    return track;
  }

  let current: Station = "todo";
  const reach = (s: Station, ts: number) => {
    track.reached[s] ??= ts;
    current = s;
  };
  let gateFailed = false;
  let reviewFailed = false;
  let landFailed = false;
  let pushFailed = false;
  let openAsks = 0;
  let rounds = 0;

  for (const e of timeline) {
    switch (e.kind) {
      case "issue_created":
        reach("todo", e.ts);
        break;
      case "claimed":
      case "worktree":
        reach("started", e.ts);
        break;
      case "status":
      case "note":
      case "handoff":
        if (track.reached.started && stationIndex(current) <= stationIndex("working")) reach("working", e.ts);
        break;
      case "gate_passed":
        gateFailed = false;
        landFailed = false;
        reach("gate", e.ts);
        break;
      case "gate_failed":
        gateFailed = true;
        reach("gate", e.ts);
        break;
      case "review_started":
        reviewFailed = false;
        landFailed = false;
        pushFailed = false;
        reach("review", e.ts);
        break;
      case "review_failed":
        reviewFailed = true;
        break;
      case "submit": {
        // A submit note means the submit got past the review (or skipped it), so an earlier
        // review that did not finish no longer holds the issue.
        reviewFailed = false;
        // Submit leaves a rejected issue In Progress without a move, so the note is the only
        // sign that the work went back from Review to Working.
        const changes = !!e.summary?.includes("requested changes");
        if (changes) {
          rounds++;
          current = "working";
        }
        // A failed push leaves the change unseen and the issue In Progress: stuck until the next
        // submit. "No pull request" alone (the push worked, opening the PR did not) can still land.
        pushFailed = !changes && !!e.summary?.includes("push failed");
        break;
      }
      case "ask":
        openAsks++;
        break;
      case "answer":
        openAsks = Math.max(0, openAsks - 1);
        break;
      case "land_failed":
        landFailed = true;
        reach("landing", e.ts);
        break;
      case "landed":
        landFailed = false;
        reach("landing", e.ts);
        reach("done", e.ts);
        break;
      case "issue_moved": {
        const to = movedTo(e.summary);
        if (!to) break;
        // A person moving the issue after a stopped land or review has taken it from there.
        landFailed = false;
        reviewFailed = false;
        // Submit itself moves the issue back to In Progress right after a failed push.
        if (to !== "In Progress") pushFailed = false;
        const s = stationOfState(to);
        if (to === "Todo" || to === "Backlog") current = "todo";
        else if (s === "working") {
          // Back from review to work: the "changes asked" loop.
          if (stationIndex(current) >= stationIndex("review")) current = "working";
          else if (!track.reached.started) reach("started", e.ts);
        } else reach(s, e.ts);
        break;
      }
    }
  }

  // The state is authoritative once an issue leaves the agent's hands; while it is In Progress,
  // the events say how far the work got.
  track.station = track.station === "working" ? (stationIndex(current) >= stationIndex("started") ? current : "working") : track.station;
  if (track.station === "landing" || track.station === "needs" || track.station === "review") {
    track.reached[track.station] ??= timeline[timeline.length - 1].ts;
  }
  track.rounds = Math.max(rounds, reviews?.filter((r) => r.verdict === "changes").length ?? 0);
  track.asking = openAsks > 0;
  const open = track.station !== "done";
  // A stopped land keeps the issue's state (often In Review), but the work is at Landing.
  if (open && landFailed) track.station = "landing";
  if (open && landFailed) track.stuck = "Land stopped";
  // A resubmit can fail without moving the issue, so these hold at whatever station it shows.
  else if (open && gateFailed) track.stuck = "Gate failed";
  else if (open && reviewFailed) track.stuck = "Review did not finish";
  else if (open && pushFailed) track.stuck = "Push failed";
  return track;
}

/** The color token of a station, given whether it is stuck. */
export function stationColor(station: Station, stuck = false): string {
  return stuck ? "var(--st-stuck)" : `var(--st-${station})`;
}

/** One line for hovers: "Review · 2 rounds of changes · stuck: Gate failed". */
export function describeTrack(t: Track): string {
  if (t.closed) return t.closed === "duplicate" ? "Duplicate" : "Canceled";
  const parts = [t.backlog ? "Backlog" : STATION_LABEL[t.station]];
  if (t.rounds) parts.push(`${t.rounds} round${t.rounds === 1 ? "" : "s"} of changes`);
  if (t.asking) parts.push("asked you something");
  if (t.stuck) parts.push(`stuck: ${t.stuck}`);
  return parts.join(" · ");
}
