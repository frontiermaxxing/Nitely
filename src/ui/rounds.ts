import type { ReviewRecord, TimelineEntry } from "../types";

/** One gate run: the playbook's gate commands, passed or failed. */
export interface GateRun {
  ok: boolean;
  ts: number;
  summary: string | null;
}

/**
 * How a round of work ended. `changes`: the review sent it back. `passed`: the review passed.
 * `unreviewed`: submitted without a review. `review-failed`: the review did not finish.
 * `reviewing`: a review is running. `open`: the agent is still at it (gates only so far).
 */
export type RoundOutcome = "changes" | "passed" | "unreviewed" | "review-failed" | "reviewing" | "open";

/** A round: everything from the work going to the gate until a submit hands it in. */
export interface Round {
  n: number;
  start: number;
  /** When the submit that closed the round was recorded; null while the round is open. */
  end: number | null;
  gates: GateRun[];
  outcome: RoundOutcome;
  reviewer: string | null;
  /** The review record the closing submit wrote, with the findings. */
  review: ReviewRecord | null;
  /** The submit could not push the branch, so nobody can see this round. */
  pushFailed: boolean;
  /** The submit handed the change to the human for approval. */
  waitsForHuman: boolean;
}

/** One land attempt. */
export interface LandRun {
  ok: boolean;
  ts: number;
  summary: string | null;
}

/** What happened between Working and Done: the rounds of gate and review, and the lands. */
export interface Journey {
  rounds: Round[];
  lands: LandRun[];
}

/** "Review by codex started" → "codex". */
function reviewerOf(summary: string | null): string | null {
  const m = summary?.match(/^Review by (\S+)/);
  return m ? m[1] : null;
}

/** A review record written up to this long after the submit note still belongs to it. */
const SKEW_SECS = 120;

/**
 * The rounds an issue went through, from its timeline and review records. A round opens with the
 * first gate or review after the last submit and closes with the next submit note; the review
 * record that submit wrote is matched to it by time. Without a timeline, each review record is a
 * round of its own.
 */
export function journeyOf(timeline: TimelineEntry[] = [], reviews: ReviewRecord[] = []): Journey {
  const rounds: Round[] = [];
  const lands: LandRun[] = [];
  let open: Round | null = null;
  const openRound = (ts: number): Round => {
    if (!open) {
      open = { n: rounds.length + 1, start: ts, end: null, gates: [], outcome: "open", reviewer: null, review: null, pushFailed: false, waitsForHuman: false };
      rounds.push(open);
    }
    return open;
  };

  for (const e of timeline) {
    switch (e.kind) {
      case "gate_passed":
      case "gate_failed":
        openRound(e.ts).gates.push({ ok: e.kind === "gate_passed", ts: e.ts, summary: e.summary });
        break;
      case "review_started": {
        const r = openRound(e.ts);
        r.outcome = "reviewing";
        r.reviewer = reviewerOf(e.summary) ?? r.reviewer;
        break;
      }
      case "review_failed": {
        const r = openRound(e.ts);
        r.outcome = "review-failed";
        r.reviewer = reviewerOf(e.summary) ?? r.reviewer;
        break;
      }
      case "submit": {
        const s = e.summary ?? "";
        const r = openRound(e.ts);
        r.outcome = s.includes("requested changes") ? "changes" : s.includes("passed") ? "passed" : "unreviewed";
        r.reviewer = reviewerOf(s) ?? r.reviewer;
        r.pushFailed = s.includes("push failed");
        r.waitsForHuman = s.includes("waits for a human");
        r.end = e.ts;
        open = null;
        break;
      }
      case "land_failed":
      case "landed":
        lands.push({ ok: e.kind === "landed", ts: e.ts, summary: e.summary });
        break;
    }
  }

  const records = [...reviews].sort((a, b) => a.ts - b.ts);
  if (rounds.length === 0) {
    // No timeline to go by (or nothing gated yet): the records alone, oldest first.
    records.forEach((rec, i) =>
      rounds.push({
        n: i + 1,
        start: rec.ts,
        end: rec.ts,
        gates: [],
        outcome: rec.verdict === "changes" ? "changes" : rec.verdict === "pass" ? "passed" : "unreviewed",
        reviewer: rec.reviewer || null,
        review: rec,
        pushFailed: false,
        waitsForHuman: rec.approver === "human",
      }),
    );
    return { rounds, lands };
  }

  // Each closed round takes the latest record written before its submit note (and after the
  // previous round's), since submit records the review just before it adds the note.
  let next = 0;
  for (const r of rounds) {
    if (r.end === null) continue;
    let match: ReviewRecord | null = null;
    while (next < records.length && records[next].ts <= r.end + SKEW_SECS) match = records[next++];
    r.review = match;
  }
  return { rounds, lands };
}

/** Findings by severity, most severe first: [[1, 2], [2, 1]] is two P1s and a P2. */
export function severityCounts(findings: ReviewRecord["findings"]): [number, number][] {
  const counts = new Map<number, number>();
  for (const f of findings) {
    const p = Math.max(0, Math.min(3, f.priority));
    counts.set(p, (counts.get(p) ?? 0) + 1);
  }
  return [...counts.entries()].sort((a, b) => a[0] - b[0]);
}

/** "2 P1, 1 P2 (1 carried over)" for hovers. */
export function describeFindings(findings: ReviewRecord["findings"]): string {
  if (findings.length === 0) return "No findings";
  const carried = findings.filter((f) => f.carried).length;
  const parts = severityCounts(findings).map(([p, n]) => `${n} P${p}`);
  return `${parts.join(", ")}${carried ? ` (${carried} carried over)` : ""}`;
}
