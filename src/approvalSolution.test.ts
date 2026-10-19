import { test } from "node:test";
import assert from "node:assert/strict";
import { approvalSolution } from "./approvalEvidence.ts";

const solution = { title: "Validate export destinations before writing", summary: "Reject paths outside the selected folder before creating the export." };
const gate = { commit: "reviewed", passed: true, results: [{ command: "npm test", ok: true }] };
test("approval text and verification belong to the same reviewed revision", () => {
  const current = approvalSolution({ commit_sha: "reviewed", solution, gate });
  assert.equal(current.title, solution.title);
  assert.equal(current.summary, solution.summary);
  assert.equal(current.gate, gate);
  const changed = approvalSolution({ commit_sha: "new", solution, gate });
  assert.equal(changed.title, "Solution summary unavailable");
  assert.equal(changed.summary, null);
  assert.equal(changed.gate, null);
});
test("legacy, missing and blank summaries are explicit without inventing the solution", () => {
  for (const evidence of [undefined, {}, { commit_sha: "reviewed", gate }, { commit_sha: "reviewed", gate, solution: { title: " ", summary: "old" } }]) {
    assert.equal(approvalSolution(evidence).title, "Solution summary unavailable");
    assert.equal(approvalSolution(evidence).summary, null);
  }
  assert.equal(approvalSolution({ commit_sha: "reviewed", gate }).gate, gate);
});
