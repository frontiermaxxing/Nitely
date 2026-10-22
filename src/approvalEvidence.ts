import type { ApprovalGate, SolutionSummary, PrRecord, TimelineEntry } from "./types";

export interface ApprovalEvidence {
  commit_sha?: string;
  solution?: SolutionSummary | null;
  gate?: ApprovalGate | null;
}

/** An intentional policy opt-out is neutral evidence, not a failed or completed review. */
export function automatedReviewEvidence(review?: { reviewer?: string; verdict?: string; note?: string | null } | null) {
  const yolo = review?.verdict === "yolo";
  const light = review?.verdict === "light";
  const quick = review?.verdict === "quick";
  const disabled = review?.verdict === "disabled" || yolo || light || quick;
  return {
    ran: !!review?.reviewer && review.verdict !== "none" && !disabled,
    text: review?.note || (yolo ? "Tests and review off (Custom mode): automatic gate and PR review skipped" : light ? "Basic checks: pre-merge review exempt; post-merge audit applies" : quick ? "Quick path: a small change, reviewed after landing" : disabled ? "Review disabled for this project" : "No automated review ran."),
    tone: disabled ? null : "bad" as const,
  };
}

export function automatedGateEvidence(gate: ApprovalGate) {
  return { ran: !gate.skipped, text: gate.skipped ? "Gate skipped" : gate.passed ? gate.plan ? `${gate.plan.depth} checks passed` : "Gate passed" : "Gate failed", reason: gate.skipped || null };
}

/** A newer skip replaces an old failure in the current-status summary, without erasing history. */
export function latestGateEvent(timeline: TimelineEntry[]) {
  return [...timeline].reverse().find((e) => e.kind === "gate_passed" || e.kind === "gate_failed" || e.kind === "gate_skipped");
}

/** Evidence is shown only for the review revision it accompanies, never from an older round. */
export function approvalSolution(evidence?: ApprovalEvidence) {
  const solution = evidence?.solution;
  const gate = evidence?.gate;
  const current = !!evidence?.commit_sha && gate?.commit === evidence.commit_sha;
  return {
    title: current && solution?.title.trim() && solution.summary.trim() ? solution.title : "Solution summary unavailable",
    summary: current && solution?.title.trim() && solution.summary.trim() ? solution.summary : null,
    gate: current ? gate : null,
    commit: evidence?.commit_sha,
  };
}

/** Conditional because landing can be run by Nitely, Crew, or the submitting agent. */
export const APPROVAL_NEXT = "Approval authorizes this submitted patch. If Nitely or Crew handles landing, it continues when checks allow; otherwise the author agent must call land. Approval alone does not merge it.";

/** Never select by updated time: an old PR can get a newer comment than this submission. */
export function approvalPr(review: { branch?: string | null; commit_sha?: string } | undefined, prs: PrRecord[] = []) {
  if (!review?.branch || !review.commit_sha || !/^[a-f0-9]{7,}$/i.test(review.commit_sha)) return undefined;
  const matches = prs.filter((p) => p.branch === review.branch && p.head_sha.startsWith(review.commit_sha!));
  return matches.length === 1 ? matches[0] : undefined;
}
