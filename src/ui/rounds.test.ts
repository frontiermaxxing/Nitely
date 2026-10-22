// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { journeyOf } from "./rounds.ts";
import type { ReviewRecord, TimelineEntry } from "../types.ts";

const ev = (ts: number, kind: string, summary = ""): TimelineEntry => ({ ts, actor: "claude:test", kind, summary }) as TimelineEntry;

test("a disabled review's passed gate is not mistaken for a passed review before records load", () => {
  const { rounds } = journeyOf([
    ev(10, "gate_passed", "Gate passed"),
    ev(20, "submit", "Review disabled for this project; gate passed and ready to land"),
  ], []);
  assert.equal(rounds[0].outcome, "unreviewed");
  assert.equal(rounds[0].reviewer, null);
  assert.equal(rounds[0].gates[0].ok, true);
});

const rec = (ts: number, commit: string, verdict: ReviewRecord["verdict"], approver: ReviewRecord["approver"] = "agent"): ReviewRecord => ({
  issue_key: "NIT-1",
  patch_id: `p-${commit}`,
  commit_sha: commit,
  reviewer: verdict === "none" ? "" : "codex",
  verdict,
  approver,
  reasons: [],
  findings: [],
  ts,
});

test("YOLO timeline skips never become passes or reuse an older real review", () => {
  const { rounds } = journeyOf([
    ev(10, "gate_failed", "Gate failed"),
    ev(20, "gate_skipped", "YOLO mode: automatic gate skipped"),
    ev(21, "submit", "YOLO mode: automatic gate and PR review skipped; earlier checks passed"),
  ], [rec(9, "old", "pass")]);
  assert.equal(rounds[0].outcome, "unreviewed");
  assert.equal(rounds[0].reviewSkipped, "yolo");
  assert.equal(rounds[0].review, null);
  assert.equal(rounds[0].reviewer, null);
  assert.deepEqual(rounds[0].gates.map((g) => [g.ok, !!g.skipped]), [[false, false], [false, true]]);
  const onlyRecord = journeyOf([], [rec(30, "new", "yolo")]).rounds[0];
  assert.equal(onlyRecord.outcome, "unreviewed");
  assert.equal(onlyRecord.reviewSkipped, "yolo");
});
test("a quick change's round is unreviewed until the review after landing, never a pass", () => {
  const { rounds } = journeyOf([
    ev(10, "gate_passed", "Gate passed"),
    ev(11, "submit", "Quick path: a small change, checked by its own checks, with no pull request; ready to land"),
  ], [rec(9, "old", "pass")]);
  assert.equal(rounds[0].outcome, "unreviewed");
  assert.equal(rounds[0].reviewSkipped, "quick");
  assert.equal(rounds[0].reviewer, null);
  assert.equal(journeyOf([], [rec(30, "new", "quick")]).rounds[0].reviewSkipped, "quick");
});
test("NIT-41: a submit that stopped after recording its review shows that review", () => {
  const timeline = [ev(10, "gate_passed", "Gate passed at aaa1111"), ev(20, "review_started", "Review by codex started")];
  const { rounds } = journeyOf(timeline, [rec(80, "aaa1111", "changes")]);
  assert.equal(rounds.length, 1);
  assert.equal(rounds[0].outcome, "changes");
  assert.equal(rounds[0].review?.commit_sha, "aaa1111");
  assert.equal(rounds[0].end, 80);
});

test("a review still running has no record yet, and an older record is not its", () => {
  const timeline = [
    ev(10, "gate_passed", "Gate passed at aaa1111"),
    ev(20, "review_started", "Review by codex started"),
    ev(60, "submit", "Review by codex requested changes. Pull request: x"),
    ev(70, "gate_passed", "Gate passed at bbb2222"),
    ev(80, "review_started", "Review by codex started"),
  ];
  const { rounds } = journeyOf(timeline, [rec(59, "aaa1111", "changes")]);
  assert.deepEqual(
    rounds.map((r) => [r.outcome, r.review?.commit_sha ?? null]),
    [
      ["changes", "aaa1111"],
      ["reviewing", null],
    ],
  );
});

test("NIT-41: two submits in one second each get their own record, by commit", () => {
  const timeline = [
    ev(10, "gate_passed", "Gate passed at aaa1111"),
    ev(50, "submit", "Submitted without a review; waits for a human. Pull request: x"),
    ev(50, "gate_passed", "Gate passed at bbb2222"),
    ev(50, "submit", "Submitted without a review; waits for a human. Pull request: x"),
  ];
  // The backend lists records newest first.
  const { rounds } = journeyOf(timeline, [rec(50, "bbb2222", "none", "human"), rec(50, "aaa1111", "none", "human")]);
  assert.deepEqual(
    rounds.map((r) => r.review?.commit_sha),
    ["aaa1111", "bbb2222"],
  );
});

test("NIT-41: a retry started in the second a stalled submit recorded keeps reviewing", () => {
  const timeline = [
    ev(10, "gate_passed", "Gate passed at aaa1111"),
    ev(20, "review_started", "Review by codex started"),
    ev(80, "gate_passed", "Gate passed at bbb2222"),
    ev(80, "review_started", "Review by codex started"),
  ];
  const { rounds } = journeyOf(timeline, [rec(80, "aaa1111", "changes")]);
  assert.equal(rounds.length, 1);
  assert.equal(rounds[0].outcome, "reviewing");
  assert.equal(rounds[0].review, null);
});
