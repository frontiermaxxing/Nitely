// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { behindSummary, held, waitLine, waitOf } from "./waits.ts";
import type { Waiting, Waits } from "./types.ts";

const NOW = 10_000;
const wait = (more: Partial<Waiting>): Waiting => ({ kind: "gate", issue: "APP-4", project: "APP", since: NOW - 300, why: "all 2 of its lanes are busy in APP", place: 1, behind: [], ...more });

test("NIT-322: a waiting request says why, and behind what, with how long each has held it", () => {
  const w = wait({
    behind: [
      { what: "run 412: APP-1 gate", issue: "APP-1", since: NOW - 360 },
      { what: "run 413: APP-2 land", issue: "APP-2", since: NOW - 3720 },
      { what: "run 414: APP-3 run", issue: "APP-3", since: null },
    ],
  });
  assert.equal(waitLine(w), "Gate waiting: all 2 of its lanes are busy in APP");
  assert.equal(behindSummary(w, NOW), "behind run 412: APP-1 gate, 6 m; run 413: APP-2 land, 1 h 2 m; 1 more");
  assert.equal(behindSummary(wait({}), NOW), null);
  assert.deepEqual([held(NOW, NOW - 40), held(NOW, NOW - 7200), held(NOW, null)], ["40 s", "2 h", null]);
  const waits: Waits = { waiting: [wait({ issue: "APP-9", kind: "land_queue" }), w] };
  assert.equal(waitOf("app-4", waits), w);
  assert.equal(waitOf("APP-5", waits), null);
  assert.equal(waitLine(waits.waiting[0]), "Land waiting: all 2 of its lanes are busy in APP");
  assert.equal(waitLine(wait({ why: "waiting for memory: 3.1 GB free" })), "Gate: waiting for memory: 3.1 GB free", "it says waiting once");
});
