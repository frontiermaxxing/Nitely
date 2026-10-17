// Review batches and the land queue as the app shows them (docs/batching.md, "What the person
// sees"): one board row per batch running, each issue's station while it is in one, the timeline
// lines of its batches, and why a batch sent a change back. Shadow batches are recorded beside the
// single review that decided, so they never change an issue's station.

import type { BatchesOverview, IssueBatches, SentBack } from "./types";

/** A batch running now, as one row on the board. */
export interface BatchRow {
  id: string;
  kind: "review" | "shadow" | "land";
  /** "Review batch · 3 changes", "Landing 3 changes". */
  title: string;
  /** Its issues, in the order the batch holds them. */
  keys: string[];
  /** "reviewing", "stacking", "gating the stack", "bisecting: gating 2 of 3", "landing". */
  step: string;
  repo: string | null;
  since: number;
}

const changes = (n: number) => `${n} change${n === 1 ? "" : "s"}`;

/** "1st", "2nd", "3rd", "4th", "11th", "22nd". */
export function ordinal(n: number): string {
  const teen = n % 100 >= 11 && n % 100 <= 13;
  const end = teen ? "th" : ({ 1: "st", 2: "nd", 3: "rd" } as Record<number, string>)[n % 10] ?? "th";
  return `${n}${end}`;
}

/** Each batch running now, review batches first (they come before a land), oldest first. */
export function batchRows(b: BatchesOverview | null): BatchRow[] {
  if (!b) return [];
  const out: BatchRow[] = [];
  for (const r of b.reviewing) {
    out.push({
      id: `review-${r.id}`,
      kind: r.shadow ? "shadow" : "review",
      title: `${r.shadow ? "Shadow review batch" : "Review batch"} · ${changes(r.issues.length)}`,
      keys: r.issues,
      step: r.shadow ? "reviewing, recorded only" : "reviewing",
      repo: r.repo_name,
      since: r.started_at,
    });
  }
  for (const q of b.land) {
    for (const run of q.runs) {
      // What the run still holds: a change it sent back, or put back in the queue, has left it.
      const keys = q.entries.filter((e) => e.run === run.id).map((e) => e.issue);
      const held = keys.length ? keys : run.issues;
      out.push({ id: `land-${run.id}`, kind: "land", title: `Landing ${changes(held.length)}`, keys: held, step: run.step ?? "stacking", repo: q.repo_name, since: run.started_at });
    }
  }
  return out;
}

/** How many changes wait in or land through the land queues now. */
export function landQueueLength(b: BatchesOverview | null): number {
  return b ? b.land.reduce((n, q) => n + q.entries.length, 0) : 0;
}

const same = (a: string, b: string) => a.toUpperCase() === b.toUpperCase();

/**
 * What the issue's station says while a batch holds it: "landing, 2nd in the queue", "landing
 * with NIT-4", "in a review batch with NIT-122, NIT-43", "waiting for a review batch". Null when
 * no batch holds it, or only a shadow one (its own review decided).
 */
export function batchStation(key: string, b: BatchesOverview | null): string | null {
  if (!b) return null;
  for (const q of b.land) {
    const at = q.entries.findIndex((e) => same(e.issue, key));
    if (at < 0) continue;
    const e = q.entries[at];
    if (e.state === "running") {
      const others = q.entries.filter((x) => x.run !== null && x.run === e.run && !same(x.issue, key)).map((x) => x.issue);
      return others.length ? `landing with ${others.join(", ")}` : "landing";
    }
    return `landing, ${ordinal(at + 1)} in the queue`;
  }
  const batch = b.reviewing.find((r) => !r.shadow && r.issues.some((k) => same(k, key)));
  if (batch) {
    const others = batch.issues.filter((k) => !same(k, key));
    return others.length ? `in a review batch with ${others.join(", ")}` : "in a review batch";
  }
  if (b.review_waiting.some((w) => !w.shadow && same(w.issue, key))) return "waiting for a review batch";
  return null;
}

/**
 * `batchStation` from the issue's own batches, for its panel: what holds it now. Null when nothing
 * does, or only a shadow batch.
 */
export function issueBatchStation(key: string, d: IssueBatches | undefined): string | null {
  if (!d) return null;
  const land = d.lands.find((l) => l.state === "queued" || l.state === "running");
  if (land?.state === "queued") return land.place ? `landing, ${ordinal(land.place)} in the queue` : "in the land queue";
  if (land) {
    const others = (land.run?.issues ?? []).filter((k) => !same(k, key) && !land.run!.sent_back.some((s) => same(s, k)));
    return others.length ? `landing with ${others.join(", ")}` : "landing";
  }
  const review = d.reviews.find((r) => !r.batch.shadow && r.state === "reviewing");
  if (review) {
    const others = review.batch.issues.filter((k) => !same(k, key));
    return others.length ? `in a review batch with ${others.join(", ")}` : "in a review batch";
  }
  return null;
}

/** Why a batch sent a change back, as the end of "sent back: ...". */
export function sentBackWords(s: SentBack): string {
  const names = s.with.join(", ");
  switch (s.why) {
    case "own":
      return "its own P0 or P1 findings";
    case "interacts":
      return `it interacts with ${names || "another change of the batch"}, which lands first`;
    case "bisected":
      return "the stack's gate failed and bisection found it";
    case "gate":
      return "its gate failed";
    case "conflict":
      return names ? `it conflicts with ${names}` : "it conflicts with the integration branch";
    case "moved":
      return "its branch changed after its review was checked";
    case "stopped":
      return "the queue run stopped";
    default:
      return "its land stopped";
  }
}

/** One line of the issue's timeline about a batch. */
export interface BatchEvent {
  ts: number;
  kind: "review_batch" | "shadow_batch" | "land_queue";
  /** Who ran it: the reviewer, or Nitely for the land queue. */
  actor: string;
  text: string;
  tone: "good" | "bad" | null;
  /** The build lane run of the gate that decided a land, for its log. */
  laneRun: number | null;
}

/** The issue's batches as timeline lines, oldest first, so they merge with its events. */
export function batchEvents(key: string, d: IssueBatches | undefined): BatchEvent[] {
  if (!d) return [];
  const out: BatchEvent[] = [];
  const others = (keys: string[]) => keys.filter((k) => !same(k, key));
  const withWords = (keys: string[]) => (others(keys).length ? ` with ${others(keys).join(", ")}` : "");
  for (const r of d.reviews) {
    const b = r.batch;
    const ts = b.ended_at ?? b.started_at;
    const verdict = r.verdict === "pass" ? "passed" : r.verdict === "changes" ? "asked for changes" : null;
    const found = r.blocking + r.minor > 0 ? ` (${[r.blocking && `${r.blocking} P0/P1`, r.minor && `${r.minor} minor`].filter(Boolean).join(", ")})` : "";
    if (b.shadow) {
      const said = verdict ? `${verdict}${found}` : r.state === "alone" ? "left it out" : b.state === "failed" ? "did not finish" : "reviewing";
      out.push({ ts, kind: "shadow_batch", actor: b.reviewer, laneRun: null, tone: null, text: `Shadow batch ${b.id}${withWords(b.issues)}: ${said}. Recorded only; its own review decided.` });
      continue;
    }
    if (r.state === "alone") {
      out.push({ ts, kind: "review_batch", actor: b.reviewer, laneRun: null, tone: null, text: `Left review batch ${b.id}${withWords(b.issues)} to be reviewed alone${r.why ? `: ${r.why}` : ""}.` });
    } else if (verdict) {
      const back = r.verdict === "changes" && r.sent_back ? `: sent back for ${sentBackWords(r.sent_back)}` : "";
      out.push({ ts, kind: "review_batch", actor: b.reviewer, laneRun: null, tone: r.verdict === "pass" ? "good" : "bad", text: `Review batch ${b.id}${withWords(b.issues)} ${verdict}${found}${back}.` });
    } else {
      out.push({ ts, kind: "review_batch", actor: b.reviewer, laneRun: null, tone: null, text: `In review batch ${b.id}${withWords(b.issues)}: reviewing.` });
    }
  }
  for (const l of d.lands) {
    const run = l.run;
    const ts = l.ended_at ?? run?.started_at ?? l.queued_at;
    const gates = run && run.gates > 0 ? ` · ${run.gates} gate${run.gates === 1 ? "" : "s"}` : "";
    const base = { ts, kind: "land_queue" as const, actor: "nitely", laneRun: l.lane_run };
    switch (l.state) {
      case "queued":
        out.push({ ...base, tone: null, text: `In the land queue${l.place ? `, ${ordinal(l.place)} in line` : ""}.` });
        break;
      case "running":
        out.push({ ...base, tone: null, text: `Landing in queue run ${run?.id ?? "?"}${withWords((run?.issues ?? []).filter((k) => !run!.sent_back.includes(k)))}${run?.step ? `: ${run.step}` : ""}.` });
        break;
      case "landed":
        // With what landed, not what the run started with: bisection or a conflict sends some back.
        out.push({ ...base, tone: "good", text: `Landed in queue run ${run?.id ?? "?"}${withWords(run?.landed.length ? run.landed : [])}${l.head ? ` at ${l.head}` : ""}${gates}.` });
        break;
      default:
        out.push({ ...base, tone: "bad", text: `Sent back from queue run ${run?.id ?? "?"}: ${sentBackWords(l.sent_back ?? { why: "other", with: [] })}${gates}.` });
    }
  }
  return out.sort((a, b) => a.ts - b.ts);
}
