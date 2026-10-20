import { test } from "node:test";
import assert from "node:assert/strict";
import { approvalPr, approvalSolution, automatedReviewEvidence, automatedGateEvidence, latestGateEvent } from "./approvalEvidence.ts";
import type { TimelineEntry } from "./types.ts";

test("Basic exemption never counts as a completed review or Full gate", () => {
  assert.deepEqual(automatedReviewEvidence({ reviewer: "", verdict: "light" }), { ran: false, text: "Basic checks: pre-merge review exempt; post-merge audit applies", tone: null });
  const evidence = automatedGateEvidence({ passed: true, commit: "abc1234", results: [], plan: { depth: "Basic", effective: "inert", head: "abc1234", policy_commit: "def5678", commands: ["lint"], reasons: [] } });
  assert.equal(evidence.text, "Basic checks passed");
});

test("YOLO remains unverified even if a stale reviewer or passing flag accompanies it", () => {
  const yolo = { reviewer: "codex", verdict: "yolo" };
  assert.deepEqual(automatedReviewEvidence(yolo), { ran: false, text: "YOLO mode: automatic gate and PR review skipped", tone: null });
  assert.deepEqual(automatedGateEvidence({ commit: "head", passed: true, skipped: "YOLO mode", results: [] }), { ran: false, text: "Gate skipped", reason: "YOLO mode" });
  const timeline = [{ ts: 1, kind: "gate_failed" }, { ts: 2, kind: "gate_skipped" }] as TimelineEntry[];
  assert.equal(latestGateEvent(timeline)?.kind, "gate_skipped");
  assert.equal([yolo, { reviewer: "codex", verdict: "pass" }].filter((r) => automatedReviewEvidence(r).ran).length, 1);
});

test("disabled review stays neutral and does not inflate reviewed rounds", () => {
  const disabled = { reviewer: "", verdict: "disabled" };
  assert.deepEqual(automatedReviewEvidence(disabled), { ran: false, text: "Review disabled for this project", tone: null });
  assert.equal(automatedReviewEvidence({ ...disabled, reviewer: "codex" }).ran, false);
  assert.equal(automatedReviewEvidence({ reviewer: "", verdict: "none" }).tone, "bad");
  const rounds = [disabled, { reviewer: "codex", verdict: "pass" }, { reviewer: "", verdict: "none" }];
  assert.equal(rounds.filter((review) => automatedReviewEvidence(review).ran).length, 1);
});

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


test("approval links follow the submitted branch and commit, not the latest PR comment", () => {
  const pr = (number: number, branch: string, head_sha: string, updated_at: string) => ({ provider: "github", repo: "example/acme", number, branch, head_sha, updated_at, title: "Change", url: `https://github.com/example/acme/pull/${number}`, state: "OPEN", comment_count: 0, approved_by: [], approved_by_me: false, checks: "success" as const });
  const old = pr(1, "claude/app-7-old", "aaaaaaa111", "2026-10-20");
  const current = pr(2, "codex/app-7-current", "bbbbbbb222", "2026-10-19");
  const review = { branch: current.branch, commit_sha: "bbbbbbb" };
  assert.equal(approvalPr(review, [old, current])?.number, 2);
  assert.equal(approvalPr(review, [old]), undefined);
  assert.equal(approvalPr(review, [{ ...current, head_sha: "ccccccc333" }]), undefined);
  assert.equal(approvalPr({ commit_sha: "bbbbbbb" }, [current]), undefined);
  assert.equal(approvalPr({ ...review, commit_sha: "" }, [current]), undefined);
  assert.equal(approvalPr(review, [current, { ...current, number: 3 }]), undefined);
});
