import { test } from "node:test";
import assert from "node:assert/strict";
import { byteText, chartBounds, chartSegments, chartValue, currentPressure, episodeExtrema, freshness, numberText, performanceQuery, sortedWorkloads } from "./performanceModel.ts";
import type { PerformanceEpisode, PerformanceMetrics, PerformancePoint, PerformanceReport, WorkloadUsage } from "./performanceModel.ts";

test("missing and zero measurements stay distinct, with binary byte units", () => {
  assert.equal(byteText(null), "Unavailable");
  assert.equal(numberText(undefined), "Unavailable");
  assert.equal(numberText(0, "%"), "0%");
  assert.equal(byteText(0), "0 B");
  assert.equal(byteText(1024 ** 3), "1 GiB");
});
const point = (at: number, cpu: number | null, gap = false): PerformancePoint => ({
  from_ms: at,
  to_ms: at,
  values: { cpu_percent: cpu } as PerformanceMetrics,
  peaks: { cpu_percent: cpu } as PerformanceMetrics,
  min_available_bytes: null,
  observations: 1,
  resolution: "sample",
  contains_gap: gap,
  overlaps_range_boundary: false,
});
test("history never draws continuity through null counters, gaps or mixed buckets", () => {
  const points = [point(0, 10), point(5, 20), point(10, null), point(15, 40), point(20, 99, true), point(25, 30), point(40, 10)];
  assert.deepEqual(
    chartSegments(points, [{ from_ms: 30, to_ms: 35, kind: "restart" }], "cpu_percent").map((s) => s.map((p) => p.to_ms)),
    [[0, 5], [15], [25], [40]],
  );
});
test("unknown workload usage sorts after measured zero without mutating the source", () => {
  const rows = [
    { id: "missing", usage: { cpu_percent: null } },
    { id: "zero", usage: { cpu_percent: 0 } },
    { id: "busy", usage: { cpu_percent: 40 } },
  ] as WorkloadUsage[];
  assert.deepEqual(
    sortedWorkloads(rows, "cpu_percent").map((r) => r.id),
    ["busy", "zero", "missing"],
  );
  assert.equal(rows[0].id, "missing");
});
test("charts preserve a short spike and low-RAM minimum separately from last values, including kernel pools", () => {
  const p = point(10, 15);
  p.peaks.cpu_percent = 99;
  p.values.physical_available_bytes = 1000;
  p.min_available_bytes = 20;
  p.values.kernel_nonpaged_bytes = 200;
  p.peaks.kernel_nonpaged_bytes = 700;
  assert.equal(chartValue(p, "cpu_percent"), 15);
  assert.equal(chartValue(p, "cpu_percent", true), 99);
  assert.equal(chartValue(p, "physical_available_bytes", true), 20);
  assert.equal(chartValue(p, "kernel_nonpaged_bytes", true), 700);
  assert.deepEqual(chartSegments([p, { ...p, to_ms: 20, from_ms: 20, contains_gap: true }], [], "cpu_percent", true), [[p]]);
});
test("restart requests do not fabricate a time and long ranges stay within the query contract", () => {
  assert.deepEqual(performanceQuery("restart", 1000), { view: "history", before_last_restart: true, limit: 400 });
  assert.equal(performanceQuery("7d", 1_000_000_000).from_ms, 395_200_000);
});
test("boundary summaries keep full intervals without moving extrema into the selected range", () => {
  const earlier = { ...point(200, 10), from_ms: 0, overlaps_range_boundary: true };
  const later = { ...point(400, 20), from_ms: 200, overlaps_range_boundary: true };
  earlier.peaks.cpu_percent = 99;
  assert.deepEqual(chartBounds([earlier, later], 100, 300), [0, 400]);
  assert.equal(earlier.from_ms, 0);
  assert.equal(later.to_ms, 400);
  assert.equal(chartValue(earlier, "cpu_percent", true), 99);
  assert.deepEqual(chartBounds([], 100, 300), [100, 300]);
});
test("episode summaries use measured ratios independently and preserve zero versus unavailable", () => {
  const episode: PerformanceEpisode = {
    from_ms: 0, to_ms: 20_000, observations: 5, observed: [], inferred_causes: [],
    peaks: { cpu_percent: 0, commit_bytes: 8 * 1024 ** 3, commit_limit_bytes: 16 * 1024 ** 3, kernel_paged_bytes: null, kernel_nonpaged_bytes: 0 } as PerformanceMetrics,
    min_available_bytes: null,
    min_available_percent: 0,
    max_commit_percent: 90,
  };
  const summary = episodeExtrema(episode);
  assert.ok(summary.includes("Peak CPU 0%"));
  assert.ok(summary.includes("Lowest available RAM Unavailable"));
  assert.ok(summary.includes("Lowest available RAM share 0%"));
  assert.ok(summary.includes("Highest commit share 90%"));
  assert.ok(summary.includes("Peak kernel paged Unavailable"));
  assert.ok(summary.includes("Peak kernel nonpaged 0 B"));
});
test("stale, paused and changed-clock samples cannot appear fresh or raise current-pressure notices", () => {
  const report = {
    settings: { enabled: true },
    latest: { wall_ms: 100_000 },
    episodes: [{ from_ms: 50_000, to_ms: 100_000, observations: 10, observed: ["cpu"], inferred_causes: [] }],
  } as unknown as PerformanceReport;
  assert.match(freshness(report, 101_000), /Recording/);
  assert.ok(currentPressure(report, 101_000));
  assert.match(freshness(report, 130_000), /Stale/);
  assert.equal(currentPressure(report, 130_000), undefined);
  assert.match(freshness(report, 90_000), /Clock changed/);
  assert.equal(currentPressure(report, 90_000), undefined);
  report.settings.enabled = false;
  assert.match(freshness(report, 101_000), /paused/);
  assert.equal(currentPressure(report, 101_000), undefined);
});
