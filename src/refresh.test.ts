// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { HIDDEN_GAP_MS, REFRESH_GAP_MS, refreshGap, waitLeft } from "./refresh.ts";

test("NIT-337: the board is read again no sooner than its last read took, and rarely while hidden", () => {
  assert.equal(refreshGap(200, false), REFRESH_GAP_MS, "a quick read: the floor");
  assert.equal(refreshGap(3800, false), 3800, "a 3.8 s snapshot waits 3.8 s: under half the time");
  assert.equal(refreshGap(3800, true), HIDDEN_GAP_MS);
  assert.equal(refreshGap(20000, true), 20000);
  assert.equal(waitLeft(10_000, null, 3800, false), 0, "the first read runs at once");
  assert.equal(waitLeft(10_000, 8_000, 3800, false), 1800);
  assert.equal(waitLeft(20_000, 8_000, 3800, false), 0);
  assert.equal(waitLeft(10_000, 8_000, 3800, true), 13_000);
});
