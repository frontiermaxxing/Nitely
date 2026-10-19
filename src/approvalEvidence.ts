import type { ApprovalGate, SolutionSummary } from "./types";

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
