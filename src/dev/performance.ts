import type { AttributionSample, PerformanceMetrics, PerformanceReport, PerformanceSettings, PerformanceUsage } from "../performanceModel";

const GiB = 1024 ** 3;
let settings: PerformanceSettings = { enabled: true, detailed_hours: 24, summary_days: 30 };
let pausedAt: number | null = null;
let cleared = false;
export const emptyMetrics = (): PerformanceMetrics => ({ cpu_percent: null, physical_total_bytes: null, physical_available_bytes: null, commit_bytes: null, commit_limit_bytes: null, kernel_paged_bytes: null, kernel_nonpaged_bytes: null, pages_input_per_sec: null, pages_output_per_sec: null, disk_read_bytes_per_sec: null, disk_write_bytes_per_sec: null, disk_latency_seconds: null, disk_queue: null });
const usage = (cpu: number, commit: number): PerformanceUsage => ({ cpu_percent: cpu, private_commit_bytes: commit * GiB, working_set_sum_bytes: commit * GiB * .7, process_read_bytes: 8 * 1024 ** 2, process_write_bytes: 1024 ** 2, processes: 4 });
function attribution(at: number, mode: string): AttributionSample {
  const partial = mode === "partial";
  return {
    wall_ms: at, monotonic_ms: at, session: "preview", boot: "preview-boot", interval_ms: 5000, collection_ms: 8, resolution: "sample", observations: 1,
    coverage: { enumerated: 190, measured: partial ? 114 : 181, denied: partial ? 70 : 4, missing_identity: 5, partial_metrics: partial ? 23 : 1, truncated: partial, first_sample: false, supported: true, short_lived_partial: true },
    nitely: usage(2.4, .2), related: usage(mode === "pressure" ? 72 : 16, 6.1), other: usage(9.1, 4), unattributed_cpu_percent: partial ? null : 4.2,
    workloads: [
      { id: "build", kind: "build", context: { project: "NIT", issue: "NIT-4", run_id: 42, job_id: null, worker_id: null, lane: 1 }, usage: usage(mode === "pressure" ? 62 : 12.4, 4.2) },
      { id: "review", kind: "review", context: { project: "NIT", issue: "NIT-7", run_id: 43, job_id: 9, worker_id: null, lane: 2 }, usage: { ...usage(3.6, 1.9), cpu_percent: partial ? null : 3.6 } },
      { id: "app", kind: "app", context: { project: null, issue: null, run_id: null, job_id: null, worker_id: null, lane: null }, usage: usage(2.4, .2) },
    ],
    top_other: [{ basename: "browser.exe", identity: { boot: "preview-boot", pid: 415, started: "preview-process" }, usage: usage(6.1, 2.4) }, { basename: "editor.exe", identity: { boot: "preview-boot", pid: 620, started: "preview-editor" }, usage: usage(3, 1.6) }],
  };
}
export function performanceFixture(args: Record<string, unknown>): PerformanceReport {
  const mode = new URLSearchParams(location.search).get("performance") ?? "idle";
  const query = (args.query ?? {}) as { view?: string; from_ms?: number; to_ms?: number; before_last_restart?: boolean };
  const now = Date.now(); const at = pausedAt ?? (mode === "stale" ? now - 180_000 : now - 1000);
  const to = query.before_last_restart ? now - 600_000 : query.to_ms ?? now;
  const from = query.before_last_restart ? to - 600_000 : query.from_ms ?? to - 3_600_000;
  const noData = cleared || mode === "empty";
  const metrics = (cpu: number): PerformanceMetrics => ({ ...emptyMetrics(), cpu_percent: cpu, physical_total_bytes: 32 * GiB, physical_available_bytes: (mode === "pressure" ? 1.8 : 13.4) * GiB, commit_bytes: (mode === "pressure" ? 46 : 22) * GiB, commit_limit_bytes: 48 * GiB, kernel_paged_bytes: .6 * GiB, kernel_nonpaged_bytes: .4 * GiB, pages_input_per_sec: mode === "pressure" ? 380 : 0, pages_output_per_sec: mode === "pressure" ? 90 : 0, disk_read_bytes_per_sec: mode === "partial" ? null : 21 * 1024 ** 2, disk_write_bytes_per_sec: 3 * 1024 ** 2, disk_latency_seconds: mode === "partial" ? null : .004, disk_queue: mode === "partial" ? null : .8 });
  const gaps = noData ? [{ from_ms: from, to_ms: to, kind: "no_retained_samples" }] : mode === "restart" || query.before_last_restart ? [{ from_ms: from + (to - from) * .4, to_ms: from + (to - from) * .52, kind: "collector_changed" }] : [];
  const points = noData ? [] : Array.from({ length: 80 }, (_, i) => {
    const time = from + (to - from) * i / 79; const v = metrics(mode === "pressure" ? 92 + Math.sin(i) * 3 : Math.max(1, 22 + Math.sin(i / 4) * 15 + (i > 40 && i < 50 ? 40 : 0)));
    return { from_ms: time, to_ms: time, values: v, peaks: { ...v, cpu_percent: Math.min(100, v.cpu_percent! + 5) }, min_available_bytes: v.physical_available_bytes, observations: 12, resolution: "minute", contains_gap: gaps.some(g => time > g.from_ms && time < g.to_ms) };
  });
  return { as_of_ms: now, from_ms: from, to_ms: to, range_anchor: query.before_last_restart ? "last_recorded_sample_of_previous_boot" : "requested_wall_time", project: null, status: !settings.enabled ? "paused" : mode === "stale" ? "stale" : noData ? "unavailable" : "fresh", age_ms: noData ? null : now - at,
    latest: noData ? null : { wall_ms: at, monotonic_ms: at, boot: "preview-boot", session: "preview", metrics: metrics(mode === "pressure" ? 97 : 31.7), collection_ms: 8 }, settings: { ...settings }, admission_limits: { total: 32 * GiB, floor: 4 * GiB, cap: 12 * GiB, floor_from: "default", cap_from: "default" },
    points: query.view === "now" ? [] : points, episodes: mode === "pressure" && !noData ? [{ from_ms: Math.floor((now - 120_000) / 300_000) * 300_000, to_ms: at, observations: 24, observed: ["cpu_above_90_percent", "physical_memory_below_10_percent"], inferred_causes: [] }] : [],
    events: noData ? [] : [{ wall_ms: to - 30_000, kind: "waiting_for_memory", project: "NIT", issue: "NIT-4", run: 42, memory_cap_bytes: 12 * GiB }],
    sessions: noData ? [] : [{ session: "preview", boot: "preview-boot", started_ms: from, stopped_ms: null }],
    coverage: { gaps, bounded: false, gaps_truncated: false, sessions_truncated: false, events_truncated: false, source_points: points.length, returned_points: query.view === "now" ? 0 : points.length, resolution: "minute", notes: ["Machine totals cover the whole PC. Null counters mean unavailable, not zero.", "Minute extrema preserve peaks; they do not establish sustained pressure within a minute."] }, units: {},
    attribution_latest: noData ? null : attribution(at, mode), attribution: noData ? [] : [attribution(to - 15_000, mode), attribution(to - 60_000, mode)],
  };
}
export function configurePerformance(args: Record<string, unknown>) {
  settings = { ...(args.settings as PerformanceSettings) };
  pausedAt = settings.enabled ? null : Date.now();
  if (settings.enabled) cleared = false;
}
export function clearPerformance() {
  if (settings.enabled) throw "Pause collection before clearing history.";
  cleared = true;
}
