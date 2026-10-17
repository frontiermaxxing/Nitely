// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { batchEvents, batchRows, batchStation, landQueueLength, ordinal, sentBackWords } from "./batches.ts";
import type { BatchesOverview, IssueBatches, LandRun, ReviewBatch } from "./types.ts";

const batch = (id: number, issues: string[], more: Partial<ReviewBatch> = {}): ReviewBatch => ({
  id, repo: "C:/Code/my-app", repo_name: "my-app", reviewer: "codex", issues, state: "reviewing", shadow: false,
  started_at: 1000, ended_at: null, seconds: null, tokens: null, error: null, ...more,
});

const run = (id: number, issues: string[], more: Partial<LandRun> = {}): LandRun => ({
  id, repo: "C:/Code/my-app", started_at: 2000, ended_at: null, issues, landed: [], sent_back: [], gates: 0, head: null, step: null, ...more,
});

/** A review batch, a shadow one, a change waiting for a batch, and a queue run of two with one behind it. */
const overview: BatchesOverview = {
  reviewing: [batch(12, ["APP-1", "APP-2", "APP-3"]), batch(13, ["APP-1", "APP-9"], { shadow: true })],
  review_waiting: [
    { issue: "APP-4", repo_name: "my-app", reviewer: "codex", shadow: false, queued_at: 900, lines: 20 },
    { issue: "APP-8", repo_name: "my-app", reviewer: "codex", shadow: true, queued_at: 900, lines: 20 },
  ],
  land: [
    {
      repo: "C:/Code/my-app",
      repo_name: "my-app",
      entries: [
        { id: 1, issue: "APP-5", state: "running", queued_at: 1900, run: 7 },
        { id: 2, issue: "APP-6", state: "running", queued_at: 1901, run: 7 },
        { id: 3, issue: "APP-7", state: "queued", queued_at: 1950, run: null },
      ],
      runs: [run(7, ["APP-5", "APP-6", "APP-10"], { step: "bisecting: gating 1 of 3", gates: 2 })],
    },
  ],
};

test("a running batch is one row on the board, a shadow one says it only records", () => {
  const rows = batchRows(overview);
  assert.deepEqual(
    rows.map((r) => [r.title, r.keys.join(" "), r.step]),
    [
      ["Review batch · 3 changes", "APP-1 APP-2 APP-3", "reviewing"],
      ["Shadow review batch · 2 changes", "APP-1 APP-9", "reviewing, recorded only"],
      // APP-10 left the run (sent back or put back in the queue): the row holds what still lands.
      ["Landing 2 changes", "APP-5 APP-6", "bisecting: gating 1 of 3"],
    ],
  );
  assert.equal(rows[2].kind, "land");
  assert.deepEqual(batchRows(null), []);
  assert.equal(landQueueLength(overview), 3);
  assert.equal(landQueueLength(null), 0);
});

test("an issue's station says which batch holds it and its place in the queue", () => {
  assert.equal(batchStation("APP-2", overview), "in a review batch with APP-1, APP-3");
  assert.equal(batchStation("app-1", overview), "in a review batch with APP-2, APP-3", "its shadow batch is not where it is");
  assert.equal(batchStation("APP-9", overview), null, "a change only in a shadow batch keeps its own station");
  assert.equal(batchStation("APP-4", overview), "waiting for a review batch");
  assert.equal(batchStation("APP-8", overview), null);
  assert.equal(batchStation("APP-5", overview), "landing with APP-6");
  assert.equal(batchStation("APP-7", overview), "landing, 3rd in the queue");
  assert.equal(batchStation("APP-11", overview), null);
  assert.deepEqual([1, 2, 3, 4, 11, 12, 13, 21, 22, 101].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st"]);
});

test("a change sent back from a batch says why", () => {
  assert.equal(sentBackWords({ why: "own", with: [] }), "its own P0 or P1 findings");
  assert.equal(sentBackWords({ why: "bisected", with: [] }), "the stack's gate failed and bisection found it");
  assert.equal(sentBackWords({ why: "conflict", with: ["APP-3"] }), "it conflicts with APP-3");
  assert.equal(sentBackWords({ why: "conflict", with: [] }), "it conflicts with the integration branch");
  assert.equal(sentBackWords({ why: "interacts", with: ["APP-2"] }), "it interacts with APP-2, which lands first");
});

test("the timeline has the batch's verdict for this change, the queue run, and a shadow batch as recorded only", () => {
  const d: IssueBatches = {
    reviews: [
      { batch: batch(13, ["APP-1", "APP-9"], { shadow: true, state: "done", ended_at: 1500 }), state: "done", verdict: "changes", blocking: 1, minor: 0, why: null, sent_back: { why: "own", with: [] } },
      { batch: batch(12, ["APP-1", "APP-2"], { state: "done", ended_at: 1200 }), state: "done", verdict: "changes", blocking: 1, minor: 2, why: null, sent_back: { why: "own", with: [] } },
    ],
    lands: [
      // APP-3 went back from run 8: the line names only what landed with it.
      { id: 3, state: "landed", queued_at: 2500, ended_at: 2600, place: null, run: run(8, ["APP-1", "APP-3", "APP-2"], { gates: 1, ended_at: 2600, landed: ["APP-1", "APP-2"], sent_back: ["APP-3"] }), lane_run: 41, head: "abc1234", error: null, sent_back: null },
      { id: 2, state: "sent_back", queued_at: 1900, ended_at: 2100, place: null, run: run(7, ["APP-1", "APP-5"], { gates: 3 }), lane_run: 40, head: null, error: "the gate failed", sent_back: { why: "bisected", with: [] } },
    ],
  };
  const lines = batchEvents("APP-1", d);
  assert.deepEqual(
    lines.map((l) => [l.kind, l.text, l.tone]),
    [
      ["review_batch", "Review batch 12 with APP-2 asked for changes (1 P0/P1, 2 minor): sent back for its own P0 or P1 findings.", "bad"],
      ["shadow_batch", "Shadow batch 13 with APP-9: asked for changes (1 P0/P1). Recorded only; its own review decided.", null],
      ["land_queue", "Sent back from queue run 7: the stack's gate failed and bisection found it · 3 gates.", "bad"],
      ["land_queue", "Landed in queue run 8 with APP-2 at abc1234 · 1 gate.", "good"],
    ],
  );
  assert.deepEqual(lines.filter((l) => l.kind === "land_queue").map((l) => l.laneRun), [40, 41], "each land links its gate's log");
  const waiting: IssueBatches = { reviews: [], lands: [{ id: 4, state: "queued", queued_at: 3000, ended_at: null, place: 2, run: null, lane_run: null, head: null, error: null, sent_back: null }] };
  assert.equal(batchEvents("APP-1", waiting)[0].text, "In the land queue, 2nd in line.");
  assert.deepEqual(batchEvents("APP-1", undefined), []);
});
