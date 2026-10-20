/** Cached recorder DTOs. Null means unavailable, never zero. All timestamps are UTC ms. */
export interface PerformanceMetrics {
  cpu_percent: number | null;
  physical_total_bytes: number | null;
  physical_available_bytes: number | null;
  commit_bytes: number | null;
  commit_limit_bytes: number | null;
  kernel_paged_bytes: number | null;
  kernel_nonpaged_bytes: number | null;
  pages_input_per_sec: number | null;
  pages_output_per_sec: number | null;
  disk_read_bytes_per_sec: number | null;
  disk_write_bytes_per_sec: number | null;
  disk_latency_seconds: number | null;
  disk_queue: number | null;
}
export interface PerformanceSettings {
  enabled: boolean;
  detailed_hours: number;
  summary_days: number;
}
export interface PerformanceSample {
  wall_ms: number;
  monotonic_ms: number;
  boot: string | null;
  session: string;
  metrics: PerformanceMetrics;
  collection_ms: number;
}
export interface PerformancePoint {
  from_ms: number;
  to_ms: number;
  values: PerformanceMetrics;
  peaks: PerformanceMetrics;
  min_available_bytes: number | null;
  observations: number;
  resolution: string;
  contains_gap: boolean;
  overlaps_range_boundary: boolean;
}
export interface PerformanceGap {
  from_ms: number;
  to_ms: number;
  kind: string;
}
export interface PerformanceEpisode {
  from_ms: number;
  to_ms: number;
  observations: number;
  observed: string[];
  inferred_causes: string[];
  peaks: PerformanceMetrics;
  min_available_bytes: number | null;
  min_available_percent: number | null;
  max_commit_percent: number | null;
}
export interface PerformanceUsage {
  cpu_percent: number | null;
  private_commit_bytes: number | null;
  working_set_sum_bytes: number | null;
  process_read_bytes: number | null;
  process_write_bytes: number | null;
  processes: number;
}
export interface WorkloadContext {
  project: string | null;
  issue: string | null;
  run_id: number | null;
  job_id: number | null;
  review_batch_id?: number | null;
  worker_id: number | null;
  lane: number | null;
}
export interface WorkloadUsage {
  id: string;
  kind: string;
  context: WorkloadContext;
  usage: PerformanceUsage;
}
export interface RecordedWorkload {
  id: string;
  kind: string;
  context: WorkloadContext;
  root: { boot: string; pid: number; started: string };
  start_ms: number;
  end_ms: number | null;
  end_observed: boolean;
  job_accounting: { cpu_100ns: number; read_bytes: number; write_bytes: number; processes: number; peak_private_commit_bytes: number | null } | null;
}
export interface AttributionSample {
  wall_ms: number;
  monotonic_ms: number;
  session: string;
  boot: string | null;
  interval_ms: number | null;
  collection_ms: number;
  resolution: string;
  observations: number;
  coverage: {
    enumerated: number;
    measured: number;
    denied: number;
    missing_identity: number;
    partial_metrics: number;
    truncated: boolean;
    first_sample: boolean;
    supported: boolean;
    short_lived_partial: boolean;
  };
  nitely: PerformanceUsage;
  related: PerformanceUsage;
  other: PerformanceUsage;
  unattributed_cpu_percent: number | null;
  workloads: WorkloadUsage[];
  top_other: { basename: string; identity: { boot: string; pid: number; started: string }; usage: PerformanceUsage }[];
}
export interface PerformanceReport {
  as_of_ms: number;
  from_ms: number;
  to_ms: number;
  range_anchor: string;
  range_boot?: string | null;
  project: string | null;
  status: string;
  age_ms: number | null;
  latest: PerformanceSample | null;
  settings: PerformanceSettings;
  admission_limits: { total: number; floor: number; cap: number; floor_from: string; cap_from: string } | null;
  points: PerformancePoint[];
  episodes: PerformanceEpisode[];
  events: { wall_ms: number; boot?: string | null; kind: string; project: string; issue: string | null; run: number; memory_cap_bytes: number | null }[];
  sessions: { session: string; boot: string | null; started_ms: number; stopped_ms: number | null }[];
  coverage: {
    gaps: PerformanceGap[];
    bounded: boolean;
    gaps_truncated: boolean;
    sessions_truncated: boolean;
    events_truncated: boolean;
    attribution_truncated: boolean;
    workloads_truncated: boolean;
    attribution_retained_bounds_ms: [number, number] | null;
    attribution_first_returned_ms: number | null;
    source_points: number;
    returned_points: number;
    boundary_points: number;
    resolution: string;
    notes: string[];
  };
  units: Record<string, unknown>;
  // Added by the attribution recorder; older histories have no attribution.
  attribution_latest: AttributionSample | null;
  attribution_age_ms: number | null;
  attribution: AttributionSample[];
  workloads: RecordedWorkload[];
  related_issues: Record<string, string>;
}
export interface PerformanceWindow { from_ms: number; to_ms: number; }
export type PerformanceRange = PerformanceWindow | "15m" | "1h" | "6h" | "24h" | "7d" | "restart";
export function performanceQuery(range: PerformanceRange, now: number): { view: "history"; from_ms?: number; to_ms?: number; before_last_restart?: boolean; limit: number } {
  return typeof range === "object" ? { view: "history", ...range, limit: 400 } : range === "restart"
    ? { view: "history", before_last_restart: true, limit: 400 }
    : {
        view: "history",
        from_ms: now - { "15m": 900_000, "1h": 3_600_000, "6h": 21_600_000, "24h": 86_400_000, "7d": 604_800_000 }[range],
        to_ms: now,
        limit: 400,
      };
}
/** Start with ten minutes so five-second attribution fits the unchanged 200-observation cap.
 * Longer incidents can be stepped through using the adjacent-window controls. */
export function incidentWindow(episode: Pick<PerformanceEpisode, "from_ms" | "to_ms">): PerformanceWindow {
  const from_ms = episode.from_ms - 30_000;
  return { from_ms, to_ms: Math.min(episode.to_ms, from_ms + 600_000) };
}
export function shiftWindow(window: PerformanceWindow, direction: -1 | 1): PerformanceWindow {
  const offset = (window.to_ms - window.from_ms) * direction;
  return { from_ms: window.from_ms + offset, to_ms: window.to_ms + offset };
}
export function earlierAttributionWindow(window: PerformanceWindow, first: number): PerformanceWindow {
  const to_ms = first - 1;
  return { from_ms: Math.max(window.from_ms, to_ms - 600_000), to_ms };
}
export function localDateTime(ms: number): string {
  const date = new Date(ms);
  return new Date(ms - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 19);
}
export function parseWindow(from: string, to: string): PerformanceWindow | null {
  const from_ms = new Date(from).getTime(), to_ms = new Date(to).getTime();
  return Number.isFinite(from_ms) && Number.isFinite(to_ms) && from_ms < to_ms ? { from_ms, to_ms } : null;
}
export function windowAttribution(report: PerformanceReport): AttributionSample | null {
  return report.attribution.reduce<AttributionSample | null>((last, sample) =>
    sample.wall_ms >= report.from_ms && sample.wall_ms <= report.to_ms && (!last || sample.wall_ms > last.wall_ms) ? sample : last, null);
}
export const numberText = (n: number | null | undefined, suffix = "", digits = 1) =>
  n == null || !Number.isFinite(n) ? "Unavailable" : `${n.toLocaleString(undefined, { maximumFractionDigits: digits })}${suffix}`;
export function byteText(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "Unavailable";
  const unit = n >= 1024 ** 3 ? 3 : n >= 1024 ** 2 ? 2 : n >= 1024 ? 1 : 0;
  return `${numberText(n / 1024 ** unit)} ${["B", "KiB", "MiB", "GiB"][unit]}`;
}
export const timestamp = (ms: number) => new Date(ms).toLocaleString();
export function freshness(report: PerformanceReport, now: number): string {
  if (!report.settings.enabled) return "Collection paused";
  if (!report.latest) return "No samples yet";
  const age = now - report.latest.wall_ms;
  if (age < -2000) return "Clock changed · sample time is ahead";
  if (age > 15_000) return `Stale · last sample ${Math.max(1, Math.floor(age / 1000))}s ago`;
  return "Recording · updated within 15s";
}
export type ChartMetric = "cpu_percent" | "physical_available_bytes" | "commit_bytes" | "kernel_paged_bytes" | "kernel_nonpaged_bytes";
/** Keep complete summary intervals visible instead of relocating their extrema into the request. */
export function chartBounds(points: PerformancePoint[], from: number, to: number): [number, number] {
  return points.reduce<[number, number]>((bounds, p) => p.overlaps_range_boundary
    ? [Math.min(bounds[0], p.from_ms), Math.max(bounds[1], p.to_ms)] : bounds, [from, to]);
}
/** Each extremum is independent; ratios use the backend's per-sample capacities, not peak ratios. */
export function episodeExtrema(episode: PerformanceEpisode): string[] {
  return [
    `Peak CPU ${numberText(episode.peaks.cpu_percent, "%")}`,
    `Lowest available RAM ${byteText(episode.min_available_bytes)}`,
    `Lowest available RAM share ${numberText(episode.min_available_percent, "%")}`,
    `Peak commit ${byteText(episode.peaks.commit_bytes)}`,
    `Highest commit share ${numberText(episode.max_commit_percent, "%")}`,
    `Peak kernel paged ${byteText(episode.peaks.kernel_paged_bytes)}`,
    `Peak kernel nonpaged ${byteText(episode.peaks.kernel_nonpaged_bytes)}`,
  ];
}
export function chartValue(point: PerformancePoint, metric: ChartMetric, extreme = false): number | null {
  return extreme ? (metric === "physical_available_bytes" ? point.min_available_bytes : point.peaks[metric]) : point.values[metric];
}
/** Break at missing counters, collector gaps and mixed buckets. Never imply continuous evidence. */
export function chartSegments(points: PerformancePoint[], gaps: PerformanceGap[], metric: ChartMetric, extreme = false): PerformancePoint[][] {
  const result: PerformancePoint[][] = [];
  let segment: PerformancePoint[] = [];
  for (const point of points) {
    const prior = segment.at(-1);
    if (point.contains_gap || chartValue(point, metric, extreme) == null || (prior && gaps.some((g) => g.from_ms < point.from_ms && g.to_ms > prior.to_ms))) {
      if (segment.length) result.push(segment);
      segment = [];
    }
    if (!point.contains_gap && chartValue(point, metric, extreme) != null) segment.push(point);
  }
  if (segment.length) result.push(segment);
  return result;
}
export function sortedWorkloads(rows: WorkloadUsage[], key: "cpu_percent" | "private_commit_bytes" | "process_read_bytes"): WorkloadUsage[] {
  return [...rows].sort((a, b) => (b.usage[key] ?? -1) - (a.usage[key] ?? -1) || a.id.localeCompare(b.id));
}
export function currentPressure(report: PerformanceReport, now: number): PerformanceEpisode | undefined {
  if (!report.settings.enabled || !report.latest || now - report.latest.wall_ms > 15_000 || now < report.latest.wall_ms - 2000) return undefined;
  return report.episodes.find((e) => e.to_ms >= report.latest!.wall_ms - 15_000 && e.to_ms - e.from_ms >= 15_000);
}
