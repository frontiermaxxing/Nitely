// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { crewOnGoal, finishLine, goalCounts, goalOrder, neededLine, riskTone, targetLine, triagedLine } from "./goals.ts";
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
  // An agent waiting on an answer counts as waiting on you.
  const asking = goalOrder([issue("A-6", "In Progress", "started")]);
  asking[0].track = { ...asking[0].track, asking: true };
  assert.equal(goalCounts(asking).needs, 1);
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

test("a goal row says who works on it and what landed today, and nothing when neither", () => {
  assert.equal(crewOnGoal({ working: 2, landed_today: 3 }), "2 working · 3 landed today");
  assert.equal(crewOnGoal({ working: 1, landed_today: 0 }), "1 working");
  assert.equal(crewOnGoal({ landed_today: 4 }), "4 landed today");
  assert.equal(crewOnGoal({ working: 0, landed_today: 0 }), null);
  assert.equal(crewOnGoal({}), null);
});

test("a goal's finish at its recent pace, late when it passes the target date", () => {
  const today = new Date(2026, 9, 18, 9, 0);
  const at = (days: number) => Math.floor(new Date(2026, 9, 18 + days, 12, 0).getTime() / 1000);
  assert.equal(finishLine(null, null, today), null, "no pace, no promise");
  assert.deepEqual(
    { ...finishLine({ at: at(4), per_day: 1 }, "2026-10-30", today)!, title: "" },
    { text: "done ~Oct 22 at this pace", late: false, title: "" },
  );
  assert.equal(finishLine({ at: at(4), per_day: 1 }, "2026-10-21", today)!.late, true);
  assert.equal(finishLine({ at: at(4), per_day: 1 }, "2026-10-22", today)!.late, false, "on the target day is not late");
  assert.equal(finishLine({ at: at(0), per_day: 3 }, null, today)!.text, "done today at this pace");
  assert.equal(finishLine({ at: at(1), per_day: 3 }, null, today)!.text, "done tomorrow at this pace");
  assert.match(finishLine({ at: at(9), per_day: 1 / 3 }, null, today)!.title, /one every 3 days/);
  // The day after a spring-forward night is tomorrow, though only 23 hours away (in a zone that
  // changes clocks; elsewhere the same).
  const march = new Date(2026, 2, 8, 9, 0);
  assert.equal(finishLine({ at: Math.floor(new Date(2026, 2, 9, 0, 30).getTime() / 1000), per_day: 1 }, null, march)!.text, "done tomorrow at this pace");
});

test("the times you were needed, in one line", () => {
  assert.equal(neededLine([]), null);
  assert.equal(neededLine([{ kind: "asked", open: false }]), "You were needed once: 1 question");
  assert.equal(
    neededLine([
      { kind: "asked", open: true },
      { kind: "approval", open: false },
      { kind: "asked", open: false },
    ]),
    "You were needed 3 times: 2 questions, 1 approval · 1 still waits",
  );
  assert.equal(neededLine([{ kind: "approval", open: true }, { kind: "approval", open: true }]), "You were needed 2 times: 2 approvals · all still wait");
});

test("a risk is coloured by the one colour rule", () => {
  assert.equal(riskTone("stuck: the gate failed"), "fail");
  assert.equal(riskTone("waits on you"), "attention");
  assert.equal(riskTone("asked you something"), "attention");
  assert.equal(riskTone("4 review rounds"), "plain");
});

test("what triage answered so you were not needed", () => {
  assert.equal(triagedLine(0), null);
  assert.equal(triagedLine(1), "Triage answered a question without you");
  assert.equal(triagedLine(4), "Triage answered 4 questions without you");
});
