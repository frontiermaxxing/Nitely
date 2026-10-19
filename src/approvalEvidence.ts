import type { ApprovalGate, SolutionSummary, PrRecord } from "./types";

export interface ApprovalEvidence {
  commit_sha?: string;
  solution?: SolutionSummary | null;
  gate?: ApprovalGate | null;
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
