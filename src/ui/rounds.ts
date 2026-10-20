import type { ReviewRecord, TimelineEntry } from "../types";

/** One gate run: the playbook's gate commands, passed or failed. */
export interface GateRun {
  ok: boolean;
  skipped?: boolean;
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
  reviewSkipped?: "yolo" | "disabled";
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

/** "Gate passed at 1a2b3c4" → "1a2b3c4". */
function gateCommitOf(summary: string | null): string | null {
  const m = summary?.match(/^Gate passed at (\S+)/);
  return m ? m[1] : null;
}

/** The outcome a review record gives its round. */
function outcomeOf(rec: ReviewRecord): RoundOutcome {
  return rec.verdict === "changes" ? "changes" : rec.verdict === "pass" ? "passed" : "unreviewed";
}

/** "Review by codex started" → "codex". */
function reviewerOf(summary: string | null): string | null {
  const m = summary?.match(/^Review by (\S+)/);
  return m ? m[1] : null;
}

/**
 * The rounds an issue went through, from its timeline and review records. A round opens with the
 * first gate or review after the last submit and closes with the next submit note; the review
 * record that submit wrote is matched to it by time, and by the commit its gate passed at when
 * two submits share a second. A submit that recorded its review but stopped before its note (the
 * push or pull request stalled) still shows that review on the open round. Without a timeline,
 * each review record is a round of its own.
 */
export function journeyOf(timeline: TimelineEntry[] = [], reviews: ReviewRecord[] = []): Journey {
  const rounds: Round[] = [];
  const lands: LandRun[] = [];
  // The round in progress, if any (a holder, so the closure below and the loop share it).
  const cur: { round: Round | null } = { round: null };
  // Per round: the commit its last passing gate ran at, and when its review started.
  const commitOf = new Map<Round, string>();
  const reviewStart = new Map<Round, number>();
  const openRound = (ts: number): Round => {
    if (!cur.round) {
      cur.round = { n: rounds.length + 1, start: ts, end: null, gates: [], outcome: "open", reviewer: null, review: null, pushFailed: false, waitsForHuman: false };
      rounds.push(cur.round);
    }
    return cur.round;
  };

  for (const e of timeline) {
    switch (e.kind) {
      case "gate_passed":
      case "gate_failed":
      case "gate_skipped":
        {
          const r = openRound(e.ts);
          r.gates.push({ ok: e.kind === "gate_passed", ...(e.kind === "gate_skipped" ? { skipped: true } : {}), ts: e.ts, summary: e.summary });
          if (e.kind === "gate_skipped") commitOf.delete(r);
          const commit = e.kind === "gate_passed" ? gateCommitOf(e.summary) : null;
          if (commit) commitOf.set(r, commit);
        }
        break;
      case "review_started": {
        const r = openRound(e.ts);
        r.outcome = "reviewing";
        r.reviewer = reviewerOf(e.summary) ?? r.reviewer;
        reviewStart.set(r, e.ts);
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
        const skipped = s.startsWith("Review disabled for this project") || s.startsWith("YOLO mode:");
        if (skipped) r.reviewSkipped = s.startsWith("YOLO mode:") ? "yolo" : "disabled";
        r.outcome = skipped ? "unreviewed" : s.includes("requested changes") ? "changes" : s.includes("passed") ? "passed" : "unreviewed";
        r.reviewer = skipped ? null : reviewerOf(s) ?? r.reviewer;
        r.pushFailed = s.includes("push failed");
        r.waitsForHuman = s.includes("waits for a human");
        r.end = e.ts;
        cur.round = null;
        break;
      }
      case "land_failed":
      case "landed":
        // Land runs the gate itself; gate runs since the last submit with no review belong to
        // the land, not to a new round of work.
        if (cur.round && cur.round.outcome === "open") {
          rounds.pop();
          cur.round = null;
        }
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
        outcome: outcomeOf(rec),
        reviewer: rec.reviewer || null,
        review: rec,
        ...(rec.verdict === "yolo" || rec.verdict === "disabled" ? { reviewSkipped: rec.verdict } : {}),
        pushFailed: false,
        waitsForHuman: rec.approver === "human",
      }),
    );
    return { rounds, lands };
  }

  // Each closed round takes the record written at or before its submit note (and after the
  // previous round's): submit records the review just before it adds the note. Of those, the one
  // for the commit the round's gate passed at, else the latest: two quick submits can share a
  // second, and then only the commit tells their records apart.
  const used = new Set<ReviewRecord>();
  let floor = -Infinity;
  const take = (r: Round, from: number, upTo: number, sameCommitOnly = false): ReviewRecord | null => {
    const fits = records.filter((rec) => !used.has(rec) && rec.ts >= Math.max(floor, from) && rec.ts <= upTo && (!r.reviewSkipped || rec.verdict === r.reviewSkipped));
    const commit = commitOf.get(r);
    const same = fits.filter((x) => commit && x.commit_sha === commit).pop();
    const rec = same ?? (sameCommitOnly ? null : fits.pop()) ?? null;
    if (rec) {
      used.add(rec);
      floor = rec.ts;
    }
    return rec;
  };
  for (const r of rounds) {
    if (r.end !== null) r.review = take(r, -Infinity, r.end);
    if (r.review?.verdict === "yolo" || r.review?.verdict === "disabled") {
      r.outcome = "unreviewed";
      r.reviewer = null;
      r.reviewSkipped = r.review.verdict;
    }
  }

  // A review is running, or its submit stopped after recording it: a record written since the
  // review started is that review's. When the round's gate named its commit, only a record for
  // that commit: a retry can start in the same second a stalled submit recorded the last one.
  const last = rounds[rounds.length - 1];
  const started = last && last.end === null && last.outcome === "reviewing" ? reviewStart.get(last) : undefined;
  if (last && started !== undefined) {
    const rec = take(last, started, Infinity, commitOf.has(last));
    if (rec) {
      last.review = rec;
      last.outcome = outcomeOf(rec);
      last.reviewer = rec.reviewer || last.reviewer;
      if (rec.verdict === "yolo" || rec.verdict === "disabled") {
        last.reviewSkipped = rec.verdict;
        last.reviewer = null;
      }
      last.waitsForHuman = rec.verdict !== "changes" && rec.approver === "human";
      last.end = rec.ts;
    }
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
