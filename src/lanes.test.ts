// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { bytesWords, capWords, chipWords, forHowLong, laneChips, laneRunId, sizeWords, waiting } from "./lanes.ts";
import type { LaneRun, ProjectLanes } from "./types.ts";

const run = (id: number, state: string, lane: number | null, more: Partial<LaneRun> = {}): LaneRun =>
  ({ id, project: "APP", repo: "C:/Code/my-app", issue: "APP-4", by: "codex", kind: "run", snapshot: "s", tree: "t", base: "b",
     command: "cargo test", state, lane, executor: "local", queued_at: 1000, started_at: lane ? 1010 : null, ended_at: null,
     exit_code: null, timed_out: false, setup_ran: false, files_switched: null, log: null, output: "", error: null, ...more }) as LaneRun;

const project = (queue: LaneRun[], recent: LaneRun[]): ProjectLanes => ({
  project: "APP",
  repo: "C:/Code/my-app",
  dir: "C:/Code/my-app-worktrees/.nitely-lanes/my-app",
  rules: { count: 3, keep: ["target"], timeout_minutes: 30 },
  lanes: [
    { lane: 1, path: "l1", made: true, busy: true, at: "1a2b3c4" },
    { lane: 2, path: "l2", made: true, busy: false, at: "5d6e7f8" },
    { lane: 3, path: "l3", made: false, busy: false, at: null },
  ],
  queue,
  recent,
});

test("each lane says what it does: running, idle, not made yet, or unhealthy", () => {
  const p = project(
    [run(9, "running", 1), run(10, "queued", null), run(11, "queued", null)],
    [run(8, "lane_error", 2, { error: "the switch failed twice: a file is held open\nmore" }), run(7, "done", 1, { exit_code: 0 })],
  );
  const chips = laneChips(p);
  assert.deepEqual(chips.map((c) => c.state), ["running", "unhealthy", "unmade"]);
  assert.equal(chipWords(chips[0], 1010 + 125), "cargo test · APP-4 · 2m");
  assert.equal(chipWords(chips[1], 0), "problem: the switch failed twice: a file is held open");
  assert.equal(chipWords(chips[2], 0), "made on its first run");
  assert.equal(waiting(p), 2);
});

test("a lane runs again after a problem, and an idle lane names the commit it holds", () => {
  const p = project([run(12, "running", 2)], [run(8, "lane_error", 2, { error: "x" })]);
  const chips = laneChips(p);
  assert.equal(chips[1].state, "running", "a running request wins over the older problem");
  assert.equal(chips[0].state, "idle");
  assert.equal(chipWords(chips[0], 0), "idle at 1a2b3c4");
});

test("a lane's last problem stays shown however many runs other lanes finish since", () => {
  const others = Array.from({ length: 10 }, (_, i) => run(20 + i, "done", 1, { exit_code: 0 }));
  const p = { ...project([], others), last: [others[0], run(8, "lane_error", 2, { error: "held open" })] };
  assert.equal(laneChips(p)[1].state, "unhealthy");
});

test("a lane's size against its cap", () => {
  const p = project([], []);
  p.lanes[0].size_bytes = 8.3 * 1024 ** 3;
  p.lanes[0].cap_bytes = 12.8 * 1024 ** 3;
  p.lanes[1].size_bytes = 640 * 1024 ** 2;
  assert.deepEqual(sizeWords(p), ["lane 1: 8.3 GB of 12.8 GB", "lane 2: 640 MB"]);
  assert.equal(capWords(p), "twice each lane's first build");
  assert.equal(capWords({ ...p, rules: { ...p.rules, size_cap_gb: 20 } }), "20 GB per lane");
  assert.equal(bytesWords(1024 ** 3), "1.0 GB");
});

test("durations and the run a timeline entry names", () => {
  assert.deepEqual([forHowLong(42), forHowLong(180), forHowLong(3900), forHowLong(7200)], ["42s", "3m", "1h 5m", "2h"]);
  assert.equal(laneRunId({ kind: "lane_run", summary: "Ran `cargo test` in lane 1: exit 0 after 42 s (run 17)" }), 17);
  assert.equal(laneRunId({ kind: "lane_run", summary: "Lane 1 could not run `x`: busy" }), null);
  assert.equal(laneRunId({ kind: "lane_run", summary: "The gate for 1a2b3c4 passed in lane run 12 already; not run again" }), 12);
  assert.equal(laneRunId({ kind: "crew_started", summary: "(run 3)" }), null);
});
