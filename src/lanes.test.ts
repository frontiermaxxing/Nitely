// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { bytesWords, capWords, chipName, chipWords, forHowLong, laneChips, laneRunId, limitsOf, machineWords, memoryWords, sizeWords, waiting } from "./lanes.ts";
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

test("in large-repo mode the chip is the main checkout: building, or waiting and why", () => {
  const big = (queue: LaneRun[]): ProjectLanes => ({
    ...project(queue, []),
    rules: { count: 1, keep: ["out"], timeout_minutes: 240, large_repo: true, base: "every 4h" },
    lanes: [{ lane: 1, path: "C:/Code/my-app", made: true, busy: false, at: "1a2b3c4" }],
  });
  const [building] = laneChips(big([run(5, "running", 1)]));
  assert.equal(chipName(building), "APP main checkout");
  assert.equal(chipWords(building, 1010 + 60), "building in your checkout: cargo test · APP-4 · 1m");
  const [held] = laneChips(big([run(6, "queued", null, { waiting_for: "your checkout has a change: notes.txt" })]));
  assert.equal(held.state, "waiting");
  assert.equal(chipWords(held, 0), "waiting: your checkout has a change: notes.txt");
  const [idle] = laneChips(big([]));
  assert.equal(chipWords(idle, 0), "idle at 1a2b3c4");
  assert.equal(chipName(laneChips(project([], []))[0]), "APP 1", "lanes keep their numbers");
});

test("durations and the run a timeline entry names", () => {
  assert.deepEqual([forHowLong(42), forHowLong(180), forHowLong(3900), forHowLong(7200)], ["42s", "3m", "1h 5m", "2h"]);
  assert.equal(laneRunId({ kind: "lane_run", summary: "Ran `cargo test` in lane 1: exit 0 after 42 s (run 17)" }), 17);
  assert.equal(laneRunId({ kind: "lane_run", summary: "Lane 1 could not run `x`: busy" }), null);
  assert.equal(laneRunId({ kind: "lane_run", summary: "The gate for 1a2b3c4 passed in lane run 12 already; not run again" }), 12);
  assert.equal(laneRunId({ kind: "crew_started", summary: "(run 3)" }), null);
});

test("the strip's heading says how many build, that one waits for memory, and that they run below normal", () => {
  const GB = 1024 ** 3;
  const machine = { memory: { total: 64 * GB, available: 6 * GB }, limits: { total: 64 * GB, floor: 12.8 * GB, cap: 16 * GB, floor_from: "default", cap_from: "settings" }, below_normal: true };
  assert.deepEqual(machineWords({ projects: [], slots: { used: 2, total: 3 }, machine, waiting_for_memory: "waiting for memory: 6.0 GB free" }), [
    "builds 2 of 3",
    "waiting for memory",
    "below normal priority",
  ]);
  assert.deepEqual(machineWords({ projects: [], slots: null }), [], "an older backend says nothing about the machine");
  assert.deepEqual(memoryWords(machine), { free: "6.0 GB of 64.0 GB free now", floor: "12.8 GB free (default)", cap: "16.0 GB per run (set here)" });
  const off = { ...machine, limits: { ...machine.limits, floor: 0, floor_from: "NITELY_MEMORY_FLOOR_GB" } };
  assert.equal(memoryWords(off).floor, "off (set by NITELY_MEMORY_FLOOR_GB)");
  assert.deepEqual(memoryWords(undefined), { free: null, floor: null, cap: null });
});

test("the floor and cap fields take GB, empty for the default", () => {
  assert.deepEqual(limitsOf("", ""), { floorGb: null, capGb: null });
  assert.deepEqual(limitsOf("0", "12.5"), { floorGb: 0, capGb: 12.5 });
  assert.match(limitsOf("-1", "") as string, /floor/);
  assert.match(limitsOf("", "0.1") as string, /cap/);
  assert.match(limitsOf("lots", "") as string, /floor/);
});
