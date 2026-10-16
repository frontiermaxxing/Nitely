import { test } from "node:test";
import assert from "node:assert/strict";
import { LONG_WAIT_SECS, STALE_SECS, livenessFrom, livenessLabel, livenessOf, waitedLong } from "./agents.ts";
import type { Session } from "../types.ts";

const NOW = 1_000_000;

function session(state: Session["state"], quietFor: number): Session {
  return { id: "s", vendor: "claude", cwd: "c:/x", started_at: 0, last_seen_at: NOW - quietFor, last_activity: null, state };
}

test("a quiet working or idle session turns stale", () => {
  assert.equal(livenessOf(session("working", 60), NOW), "working");
  assert.equal(livenessOf(session("working", STALE_SECS + 1), NOW), "stale");
  assert.equal(livenessOf(session("idle", STALE_SECS + 1), NOW), "stale");
});

test("a waiting session never turns stale, however long the human takes", () => {
  assert.equal(livenessOf(session("waiting", STALE_SECS + 1), NOW), "waiting");
  assert.equal(livenessOf(session("waiting", 10 * 3600), NOW), "waiting");
  assert.equal(livenessFrom("waiting", 0, NOW), "waiting");
});

test("a long wait is marked, and says how long", () => {
  assert.equal(waitedLong(session("waiting", LONG_WAIT_SECS - 1), NOW), false);
  assert.equal(waitedLong(session("waiting", LONG_WAIT_SECS), NOW), true);
  assert.equal(waitedLong(session("idle", LONG_WAIT_SECS * 4), NOW), false);
  assert.equal(livenessLabel(session("waiting", 60), NOW), "waiting for you");
  assert.equal(livenessLabel(session("waiting", 40 * 60), NOW), "waiting for you 40m");
  assert.equal(livenessLabel(session("waiting", 5 * 3600), NOW), "waiting for you 5h");
});

test("ended stays ended and no session has no liveness", () => {
  assert.equal(livenessOf(session("ended", 5), NOW), "ended");
  assert.equal(livenessOf(null, NOW), null);
  assert.equal(livenessLabel(null, NOW), "no session");
});
