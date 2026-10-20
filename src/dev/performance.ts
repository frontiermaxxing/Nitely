import type { AttributionSample, PerformanceMetrics, PerformanceReport, PerformanceSettings, PerformanceUsage } from "../performanceModel";

const GiB = 1024 ** 3;
let settings: PerformanceSettings = { enabled: true, detailed_hours: 24, summary_days: 30 };
let pausedAt: number | null = null;
let cleared = false;
export const emptyMetrics = (): PerformanceMetrics => ({
  cpu_percent: null,
  physical_total_bytes: null,
  physical_available_bytes: null,
  commit_bytes: null,
  commit_limit_bytes: null,
  kernel_paged_bytes: null,
  kernel_nonpaged_bytes: null,
  pages_input_per_sec: null,
  pages_output_per_sec: null,
  disk_read_bytes_per_sec: null,
  disk_write_bytes_per_sec: null,
  disk_latency_seconds: null,
  disk_queue: null,
});
const usage = (cpu: number, commit: number): PerformanceUsage => ({
  cpu_percent: cpu,
  private_commit_bytes: commit * GiB,
  working_set_sum_bytes: commit * GiB * 0.7,
  process_read_bytes: 8 * 1024 ** 2,
  process_write_bytes: 1024 ** 2,
  processes: 4,
});
function attribution(at: number, mode: string, boot = "preview-boot"): AttributionSample {
  const partial = mode === "partial";
  return {
    wall_ms: at,
    monotonic_ms: at,
    session: boot === "preview-boot" ? "preview" : "preview-before-restart",
    boot,
    interval_ms: 5000,
    collection_ms: 8,
    resolution: "sample",
    observations: 1,
    coverage: {
      enumerated: 190,
      measured: partial ? 114 : 181,
      denied: partial ? 70 : 4,
      missing_identity: 5,
      partial_metrics: partial ? 23 : 1,
      truncated: partial,
      first_sample: false,
      supported: true,
      short_lived_partial: true,
    },
    nitely: usage(2.4, 0.2),
    related: usage(mode === "pressure" ? 72 : 16, 6.1),
    other: usage(9.1, 4),
    unattributed_cpu_percent: partial ? null : 4.2,
    workloads: [
      {
        id: "build",
        kind: "build",
        context: { project: "NIT", issue: "NIT-4", run_id: 42, job_id: null, worker_id: null, lane: 1 },
        usage: usage(mode === "pressure" ? 62 : 12.4, 4.2),
      },
      {
        id: "review",
        kind: "review",
        context: { project: "NIT", issue: "NIT-7", run_id: 43, job_id: 9, worker_id: null, lane: 2 },
        usage: { ...usage(3.6, 1.9), cpu_percent: partial ? null : 3.6 },
      },
      { id: "app", kind: "app", context: { project: null, issue: null, run_id: null, job_id: null, worker_id: null, lane: null }, usage: usage(2.4, 0.2) },
    ],
    top_other: [
      { basename: "browser.exe", identity: { boot, pid: 415, started: "preview-process" }, usage: usage(6.1, 2.4) },
      { basename: "editor.exe", identity: { boot, pid: 620, started: "preview-editor" }, usage: usage(3, 1.6) },
    ],
  };
}
export function performanceFixture(args: Record<string, unknown>): PerformanceReport {
  const mode = new URLSearchParams(location.search).get("performance") ?? "idle";
  if (mode === "error") throw "Performance history is temporarily unavailable (preview).";
  const query = (args.query ?? {}) as { view?: string; from_ms?: number; to_ms?: number; before_last_restart?: boolean };
  const now = Date.now();
  const at = pausedAt ?? (mode === "stale" ? now - 180_000 : now - 1000);
  const to = query.before_last_restart ? now - 600_000 : (query.to_ms ?? now);
  const from = query.before_last_restart ? to - 600_000 : (query.from_ms ?? to - 3_600_000);
  const rangeBoot = query.before_last_restart ? "preview-previous-boot" : "preview-boot";
  const noData = cleared || mode === "empty";
  const metrics = (cpu: number): PerformanceMetrics => ({
    ...emptyMetrics(),
    cpu_percent: cpu,
    physical_total_bytes: 32 * GiB,
    physical_available_bytes: (mode === "pressure" ? 1.8 : 13.4) * GiB,
    commit_bytes: (mode === "pressure" ? 46 : 22) * GiB,
    commit_limit_bytes: 48 * GiB,
    kernel_paged_bytes: 0.6 * GiB,
    kernel_nonpaged_bytes: 0.4 * GiB,
    pages_input_per_sec: mode === "pressure" ? 380 : 0,
    pages_output_per_sec: mode === "pressure" ? 90 : 0,
    disk_read_bytes_per_sec: mode === "partial" ? null : 21 * 1024 ** 2,
    disk_write_bytes_per_sec: 3 * 1024 ** 2,
    disk_latency_seconds: mode === "partial" ? null : 0.004,
    disk_queue: mode === "partial" ? null : 0.8,
  });
  const gaps = noData
    ? [{ from_ms: from, to_ms: to, kind: "no_retained_samples" }]
    : mode === "restart" || query.before_last_restart
      ? [{ from_ms: from + (to - from) * 0.4, to_ms: from + (to - from) * 0.52, kind: "collector_changed" }]
      : [];
  const points = noData
    ? []
    : Array.from({ length: 80 }, (_, i) => {
        const time = from + ((to - from) * i) / 79;
        const v = metrics(mode === "pressure" ? 92 + Math.sin(i) * 3 : Math.max(1, 22 + Math.sin(i / 4) * 15 + (i > 40 && i < 50 ? 40 : 0)));
        return {
          from_ms: i === 0 ? time - 30_000 : time,
          to_ms: i === 79 ? time + 30_000 : time,
          values: v,
          peaks: {
            ...v,
            cpu_percent: i === 30 ? 99 : Math.min(100, v.cpu_percent! + 5),
            kernel_paged_bytes: (i === 30 ? 2 : 0.7) * GiB,
            kernel_nonpaged_bytes: (i === 30 ? 1.4 : 0.5) * GiB,
          },
          min_available_bytes: i === 30 ? 0.8 * GiB : v.physical_available_bytes,
          observations: 12,
          resolution: "minute",
          contains_gap: gaps.some((g) => time > g.from_ms && time < g.to_ms),
          overlaps_range_boundary: i === 0 || i === 79,
        };
      });
  return {
    as_of_ms: now,
    from_ms: from,
    to_ms: to,
    range_anchor: query.before_last_restart ? "last_recorded_sample_of_previous_boot" : "requested_wall_time",
    range_boot: query.before_last_restart ? rangeBoot : null,
    project: null,
    status: !settings.enabled ? "paused" : mode === "stale" ? "stale" : noData ? "unavailable" : "fresh",
    age_ms: noData ? null : now - at,
    latest: noData
      ? null
      : { wall_ms: at, monotonic_ms: at, boot: "preview-boot", session: "preview", metrics: metrics(mode === "pressure" ? 97 : 31.7), collection_ms: 8 },
    settings: { ...settings },
    admission_limits: { total: 32 * GiB, floor: 4 * GiB, cap: 12 * GiB, floor_from: "default", cap_from: "default" },
    points: query.view === "now" ? [] : points,
    episodes:
      mode === "pressure" && !noData
        ? [
            {
              from_ms: query.before_last_restart ? from : Math.floor((now - 120_000) / 300_000) * 300_000,
              to_ms: query.before_last_restart ? to : at,
              observations: 24,
              observed: ["CPU at least 90%", "available physical memory at most 10%"],
              inferred_causes: [],
              peaks: { ...metrics(99), commit_bytes: 47.52 * GiB, kernel_paged_bytes: 2 * GiB, kernel_nonpaged_bytes: 1.4 * GiB },
              min_available_bytes: 0.8 * GiB,
              min_available_percent: 2.5,
              max_commit_percent: 99,
            },
          ]
        : [],
    events: noData ? [] : [{ wall_ms: to - 30_000, boot: rangeBoot, kind: "waiting_for_memory", project: "NIT", issue: "NIT-4", run: 42, memory_cap_bytes: 12 * GiB }],
    sessions: noData ? [] : [{ session: query.before_last_restart ? "preview-before-restart" : "preview", boot: rangeBoot, started_ms: from, stopped_ms: query.before_last_restart ? to : null }],
    coverage: {
      gaps,
      bounded: false,
      gaps_truncated: false,
      sessions_truncated: false,
      events_truncated: false,
      attribution_truncated: false,
      workloads_truncated: false,
      attribution_retained_bounds_ms: noData ? null : [from, at],
      attribution_first_returned_ms: noData ? null : to - 60_000,
      source_points: points.length,
      returned_points: query.view === "now" ? 0 : points.length,
      boundary_points: noData || query.view === "now" ? 0 : 2,
      resolution: "minute",
      notes: [
        "Machine totals cover the whole PC. Null counters mean unavailable, not zero.",
        "Minute extrema preserve peaks; they do not establish sustained pressure within a minute.",
      ],
    },
    units: {},
    attribution_latest: noData ? null : attribution(at, mode),
    attribution: noData ? [] : [attribution(to - 15_000, mode, rangeBoot), attribution(to - 60_000, mode, rangeBoot)],
    attribution_age_ms: noData ? null : now - at,
    related_issues: { "NIT-4": "http://127.0.0.1:7423/i/NIT-4" },
    workloads: noData
      ? []
      : [
          {
            id: "short-build",
            kind: "build",
            context: { project: "NIT", issue: "NIT-4", run_id: 41, job_id: null, worker_id: null, lane: 1 },
            root: { boot: rangeBoot, pid: 100, started: "preview" },
            start_ms: to - 120_000,
            end_ms: to - 119_000,
            end_observed: false,
            job_accounting: { cpu_100ns: 7_000_000, read_bytes: 2 * 1024 ** 2, write_bytes: 1024, processes: 3, peak_private_commit_bytes: 0.4 * GiB },
          },
        ],
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
