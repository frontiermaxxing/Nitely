// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { goalCounts, goalOrder, targetLine } from "./goals.ts";
import type { IssueRecord } from "./types.ts";

const issue = (key: string, state: string, state_type: string): IssueRecord =>
  ({ key, state, state_type, origin: "nitely", title: key, body: "", url: "", priority: 0, labels: [], evidence: "", created_at: "", updated_at: "", blocked_by: [], blocks: [] }) as unknown as IssueRecord;

test("a goal's issues run from done to queued, closed ones left out", () => {
  const order = goalOrder([
    issue("NIT-9", "Todo", "unstarted"),
    issue("NIT-3", "Done", "completed"),
    issue("NIT-5", "Needs You", "started"),
    issue("NIT-7", "In Progress", "started"),
    issue("NIT-8", "Canceled", "canceled"),
    issue("NIT-2", "Backlog", "backlog"),
    issue("NIT-4", "In Review", "started"),
    issue("NIT-1", "Done", "completed"),
  ]).map((p) => p.issue.key);
  assert.deepEqual(order, ["NIT-1", "NIT-3", "NIT-5", "NIT-4", "NIT-7", "NIT-9", "NIT-2"]);
});

test("counts follow the colour rule", () => {
  const c = goalCounts(goalOrder([issue("A-1", "Done", "completed"), issue("A-2", "Needs You", "started"), issue("A-3", "In Progress", "started"), issue("A-4", "Todo", "unstarted"), issue("A-5", "Backlog", "backlog")]));
  assert.deepEqual(c, { done: 1, moving: 1, needs: 1, stuck: 0, queued: 2 });
});

test("the target date says how far off or how late it is", () => {
  const today = new Date(2026, 9, 17);
  assert.equal(targetLine(null, today, false), null);
  assert.deepEqual(targetLine("2026-10-29", today, false), { text: "by Oct 29 · in 12 days", late: false });
  assert.deepEqual(targetLine("2026-10-17", today, false), { text: "by Oct 17 · today", late: false });
  assert.deepEqual(targetLine("2026-10-14", today, false), { text: "by Oct 14 · 3 days late", late: true });
  assert.deepEqual(targetLine("2026-10-14", today, true), { text: "by Oct 14", late: false });
  assert.equal(targetLine("2027-01-05", today, false)?.text, "by Jan 5, 2027 · in 80 days");
});
