import { test } from "node:test";
import assert from "node:assert/strict";
import { phoneProjects, shownIndex } from "./select.ts";

test("NIT-236: the phone filters by project, and goes back to all once the project has nothing left", () => {
  const keys = ["APP-1", "WEB-2", "APP-3", null];
  assert.deepEqual(phoneProjects(keys, null), { counts: [{ project: "APP", count: 2 }, { project: "WEB", count: 1 }], project: null });
  assert.equal(phoneProjects(keys, "web").project, "WEB");
  assert.equal(phoneProjects(["APP-1"], "WEB").project, null);
});

test("the shown decision stays on screen when one is added before it", () => {
  assert.equal(shownIndex(["new", "a", "b"], { id: "a", index: 0 }), 1);
});

test("when the shown decision goes, the one after it takes its place", () => {
  assert.equal(shownIndex(["a", "c"], { id: "b", index: 1 }), 1);
  // The last one goes: the one before it.
  assert.equal(shownIndex(["a"], { id: "b", index: 1 }), 0);
  assert.equal(shownIndex([], { id: "b", index: 1 }), 0);
});

test("a reply for another decision does not move the screen off the one being written (NIT-152)", () => {
  // A was approved, the person moved on to B and is writing; A's reply refreshes the list.
  assert.equal(shownIndex(["b", "c"], { id: "b", index: 1 }), 0);
});

test("after an insert and a removal, the position followed the shown one (NIT-152)", () => {
  // B was at 0, an ask arrived before it (B pinned at 1), then B went: the next one, not the ask.
  assert.equal(shownIndex(["ask", "c"], { id: "b", index: 1 }), 1);
});
