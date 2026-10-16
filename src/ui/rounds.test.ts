// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { journeyOf } from "./rounds.ts";
import type { ReviewRecord, TimelineEntry } from "../types.ts";

const ev = (ts: number, kind: string, summary = ""): TimelineEntry => ({ ts, actor: "claude:test", kind, summary }) as TimelineEntry;

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
