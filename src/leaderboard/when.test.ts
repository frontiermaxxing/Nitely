import { test } from "node:test";
import assert from "node:assert/strict";
import { when } from "./when.ts";

// Local times, so the test reads the same in any time zone.
const at = (day: number, hour: number) => new Date(2026, 9, day, hour, 0, 0).getTime() / 1000;

test("a send's time reads as now, today, tomorrow, or its weekday (NIT-629)", () => {
  const monday = at(19, 10); // Monday 2026-10-19, 10:00
  assert.equal(when(monday, monday), "now");
  assert.equal(when(monday, monday - 3600), "now", "a send that is due reads as now");
  assert.equal(when(monday, at(19, 18)), "today 18:00");
  assert.equal(when(monday, at(20, 18)), "tomorrow 18:00");
  assert.equal(when(monday, at(25, 18)), "Sun 18:00");
});
