import { memo, useEffect, useRef, useState } from "react";
import { invoke, isLive } from "./backend";
import { Loading, EmptyState, Select } from "./ui";
import { performanceClock, pollPerformance } from "./performancePolling";
import type { LanesOverview, MachinePace, PaceAdjustment } from "./types";
import { HEALTHY_START_MS, paceHeadline } from "./lanes";
import { byteText, chartBounds, chartSegments, chartValue, currentPressure, episodeExtrema, earlierAttributionWindow, incidentWindow, localDateTime, parseWindow, shiftWindow, windowAttribution, freshness, numberText, performanceQuery, sortedWorkloads, timestamp } from "./performanceModel";
import type {
  AttributionSample,
  ChartMetric,
  PerformanceRange,
  PerformanceWindow,
  PerformanceReport,
  PerformanceSettings,
  PerformanceUsage,
  WorkloadContext,
} from "./performanceModel";
import "./performance.css";

const RANGES: Exclude<PerformanceRange, PerformanceWindow>[] = ["15m", "1h", "6h", "24h", "7d", "restart"];
const RANGE_LABEL = (r: Exclude<PerformanceRange, PerformanceWindow>) => (r === "restart" ? "Before last restart" : r);

/** Reads the recorder's cache; opening this page never creates another collector. */
export function Performance(props: { onOpenIssue: (key: string) => void; onSettings: () => void }) {
  const [range, setRange] = useState<PerformanceRange>("1h");
  const [history, setHistory] = useState<PerformanceReport | null>(null);
  const [current, setCurrent] = useState<PerformanceReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [currentError, setCurrentError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [now, setNow] = useState(Date.now());
  const [dismissed, setDismissed] = useState(false);
  const [lanes, setLanes] = useState<LanesOverview | null>(null);
  const fixed = typeof range === "object" || range === "restart";
  const rangeKey = typeof range === "object" ? `${range.from_ms}/${range.to_ms}` : range;
  useEffect(() => {
    let live = true;
    const stop = pollPerformance(async () => {
      try {
        const report = await invoke<PerformanceReport>("performance_query", { query: performanceQuery(range, Date.now()) });
        if (live) { setHistory(report); setError(null); }
      } catch (e) { if (live) setError(String(e)); }
    }, fixed ? null : 60_000, performanceClock);
    return () => { live = false; stop(); };
  }, [range, reload]);
  useEffect(() => {
    let live = true;
    const stop = pollPerformance(async () => {
      try {
        const report = await invoke<PerformanceReport>("performance_query", { query: { view: "now", limit: 200 } });
        if (live) { setCurrent(report); setCurrentError(null); }
      } catch (e) { if (live) setCurrentError(String(e)); }
    }, 5000, performanceClock);
    const clock = setInterval(() => { if (!document.hidden) setNow(Date.now()); }, 1000);
    return () => { live = false; stop(); clearInterval(clock); };
  }, [reload]);
  useEffect(() => {
    let live = true;
    const stop = pollPerformance(async () => {
      try { const result = await invoke<LanesOverview>("lanes_overview"); if (live) setLanes(result); }
      catch { if (live) setLanes(null); }
    }, 30_000, performanceClock);
    return () => { live = false; stop(); };
  }, [reload]);
  const data = history
    ? {
        ...history,
        ...(current
          ? {
              latest: current.latest,
              settings: current.settings,
              status: current.status,
              admission_limits: current.admission_limits,
              pace: current.pace ?? history.pace,
              pace_measured_ms: current.pace_measured_ms ?? history.pace_measured_ms,
              attribution_latest: current.attribution_latest,
              attribution_age_ms: current.attribution_age_ms,
            }
          : {}),
      }
    : current;
  const chooseRange = (r: PerformanceRange) => {
    if (r !== range) {
      setRange(r);
      setHistory(null);
      setError(null);
      requestAnimationFrame(() => document.getElementById("performance-history")?.scrollIntoView({ block: "start" }));
    }
  };
  const metrics = data?.latest?.metrics;
  const pressure = current && currentPressure(current, now);
  useEffect(() => {
    if (current && !currentPressure(current, Date.now())) setDismissed(false);
  }, [current]);
  return (
    <div className="page perf" data-testid="performance-page">
      <div className="page-head">
        <h1>Performance</h1>
        <span className="head-glance">This machine · all projects</span>
      </div>
      <p className="perf-intro">Machine pressure, the work using it, and what happened before a slowdown.</p>
      {error && (
        <p className="error" role="alert">
          {error}{" "}
          <button className="link" onClick={() => setReload((n) => n + 1)}>
            Try again
          </button>
        </p>
      )}
      {currentError && currentError !== error && (
        <p className="error" role="alert">
          Current readings: {currentError}
        </p>
      )}
      {!data && !error && <Loading />}
      {data && (
        <>
          <div className="perf-freshness">
            <span
              className={
                data.settings.enabled && data.latest && now - data.latest.wall_ms >= -2000 && now - data.latest.wall_ms <= 15_000 ? "live-ink" : "muted"
              }
            >
              {freshness(data, now)}
            </span>
            <span className="muted" title={data.latest ? timestamp(data.latest.wall_ms) : undefined}>
              {data.latest ? `Last sample ${timestamp(data.latest.wall_ms)}` : "Collection may take a few seconds to start."}
            </span>
          </div>
          {pressure && !dismissed && (
            <div className="perf-pressure" role="status">
              <div>
                <strong>Sustained pressure observed</strong>
                <p>
                  {pressure.observed.map((s) => s.replaceAll("_", " ")).join(" · ")} since {timestamp(pressure.from_ms)}. Workload overlap can help investigate;
                  it does not establish the cause.
                </p>
              </div>
              <button className="link" onClick={() => setDismissed(true)}>
                Dismiss
              </button>
            </div>
          )}
          <div className="perf-metrics">
            <Metric label="CPU" value={numberText(metrics?.cpu_percent, "%")} detail="Whole machine" />
            <Metric
              label="Available RAM"
              value={byteText(metrics?.physical_available_bytes)}
              detail={`of ${byteText(metrics?.physical_total_bytes)} physical RAM`}
            />
            <Metric label="Committed memory" value={byteText(metrics?.commit_bytes)} detail={`limit ${byteText(metrics?.commit_limit_bytes)}`} />
            <Metric
              label="Paging"
              value={numberText(metrics?.pages_input_per_sec, " pages/s")}
              detail={`out ${numberText(metrics?.pages_output_per_sec, " pages/s")}`}
            />
            <Metric
              label="Disk activity"
              value={metrics?.disk_read_bytes_per_sec == null ? "Unavailable" : `${byteText(metrics.disk_read_bytes_per_sec)}/s read`}
              detail={metrics?.disk_write_bytes_per_sec == null ? "Write rate unavailable" : `${byteText(metrics.disk_write_bytes_per_sec)}/s write`}
            />
            <Metric
              label="Disk pressure"
              value={metrics?.disk_latency_seconds == null ? "Unavailable" : numberText(metrics.disk_latency_seconds * 1000, " ms latency")}
              detail={`queue ${numberText(metrics?.disk_queue)}`}
            />
          </div>
          <p className="muted small">
            Counters are measured when supported. Disk activity is transfer traffic, not free disk space. Kernel memory: {byteText(metrics?.kernel_paged_bytes)}{" "}
            paged · {byteText(metrics?.kernel_nonpaged_bytes)} nonpaged.
          </p>
          <Pace pace={data.pace ?? lanes?.machine?.pace} measuredMs={data.pace_measured_ms} nonpaged={metrics?.kernel_nonpaged_bytes} adjustments={history?.adjustments ?? []} />
        </>
      )}
      <section className="perf-section" id="performance-history">
        <div className="perf-heading">
          <h2>History</h2>
          <div className="perf-ranges" role="group" aria-label="Performance time range">
            {RANGES.map((r) => (
              <button key={r} aria-pressed={range === r} onClick={() => chooseRange(r)}>
                {RANGE_LABEL(r)}
              </button>
            ))}
          </div>
        </div>
        <HistoryWindow key={`${rangeKey}/${history ? "ready" : "loading"}`} window={typeof range === "object" ? range : history ?? { from_ms: now - 3_600_000, to_ms: now }}
          fixed={fixed} onChoose={chooseRange} onRefresh={() => setReload((n) => n + 1)} />
        {!history && !error && <Loading />}
        {history && (
          <>
            <HistoryChart data={history} />
            <p className="muted small">
              {timestamp(history.from_ms)} – {timestamp(history.to_ms)} · {history.coverage.returned_points} of {history.coverage.source_points} points ·{" "}
              {history.coverage.resolution.replaceAll("_", " ")}
            </p>
            {range === "restart" && <p className="small">Ends at the last retained sample of the previous boot. The exact shutdown time is unknown.</p>}
            {history.coverage.boundary_points > 0 && <p className="small perf-boundary-note">
              {history.coverage.boundary_points} source minute intervals overlap the selected range boundary. Their full intervals are retained;
              last values and extrema may fall outside the exact selected timestamps. The chart and table preserve their actual bounds.
            </p>}
            <details className="perf-details">
              <summary>Collection coverage · {history.coverage.gaps.length} recorded gaps</summary>
              <ul>
                {history.coverage.gaps.map((g, i) => (
                  <li key={i}>
                    {g.kind.replaceAll("_", " ")} · {timestamp(g.from_ms)} – {timestamp(g.to_ms)}
                  </li>
                ))}
              </ul>
              {history.coverage.notes.map((n) => (
                <p className="muted small" key={n}>
                  {n}
                </p>
              ))}
              {(history.coverage.bounded || history.coverage.gaps_truncated || history.coverage.sessions_truncated || history.coverage.events_truncated) && (
                <p>Results are bounded; some detail is omitted. Narrow the time range to inspect it.</p>
              )}
            </details>
          </>
        )}
      </section>
      {data && (
        <>
          {history && <Workloads key={`${rangeKey}/${reload}`} data={data} fixed={fixed} now={now} onOpenIssue={props.onOpenIssue} onWindow={chooseRange} />}
          <section className="perf-section">
            <h2>Pressure episodes & workload events</h2>
            {history && !history.episodes.length && <p className="muted">No sustained pressure established in the retained detailed samples for this range.</p>}
            {history?.episodes.map((e) => (
              <div className="perf-episode" key={e.from_ms}>
                <button className="link perf-inspect" onClick={() => chooseRange(incidentWindow(e))}>Inspect incident</button>
                <strong>{e.observed.map((s) => s.replaceAll("_", " ")).join(" · ")}</strong>
                <span>
                  {timestamp(e.from_ms)} – {timestamp(e.to_ms)}
                </span>
                <span className="muted">
                  {e.observations} observations{e.inferred_causes.length ? ` · possible causes: ${e.inferred_causes.join(", ")}` : " · cause not established"}
                </span>
                <p className="small">{episodeExtrema(e).join(" · ")}</p>
                <span className="muted small">Measured extrema may come from different observations within this episode.</span>
              </div>
            ))}
            <details className="perf-details">
              <summary>{history?.events.length ?? 0} workload events in this window</summary>
              {history?.events.length ? (
                <ul>
                  {history.events.map((e, i) => (
                    <li key={i}>
                      {timestamp(e.wall_ms)} · {e.kind.replaceAll("_", " ")} · {e.project} ·{" "}
                      <button className="link" onClick={() => invoke("open_lane_log", { id: e.run }).catch((err) => setError(String(err)))}>
                        run {e.run}
                      </button>
                      {e.issue && (
                        <>
                          {" "}
                          ·{" "}
                          <button className="link" onClick={() => props.onOpenIssue(e.issue!)}>
                            {e.issue}
                          </button>
                        </>
                      )}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="muted">No recorded workload transitions in this window.</p>
              )}
            </details>
          </section>
          <section className="perf-section">
            <div className="perf-heading">
              <h2>Admission settings</h2>
              <button className="link" onClick={props.onSettings}>
                Open build lane settings
              </button>
            </div>
            <p>
              {lanes?.slots ? `${lanes.slots.used} of ${lanes.slots.total} build slots in use. ` : "Build slot usage unavailable. "}Memory floor{" "}
              {byteText(data.admission_limits?.floor)} · per-run cap {byteText(data.admission_limits?.cap)}.
            </p>
            <p className="muted small">
              Current settings, using the recorder's cached memory total. New-work admission checks live memory separately. This page does not stop work or
              change concurrency.
            </p>
            {lanes?.waiting_for_memory && <p>{lanes.waiting_for_memory}</p>}
          </section>
          <RecorderSettings settings={data.settings} onChanged={() => setReload((n) => n + 1)} />
        </>
      )}
    </div>
  );
}

function HistoryWindow({ window, fixed, onChoose, onRefresh }: {
  window: PerformanceWindow; fixed: boolean; onChoose: (window: PerformanceWindow) => void; onRefresh: () => void;
}) {
  const [from, setFrom] = useState(() => localDateTime(window.from_ms));
  const [to, setTo] = useState(() => localDateTime(window.to_ms));
  const [error, setError] = useState(false);
  return <div className="perf-window">
    <div className="perf-window-actions">
      <button className="btn" onClick={() => onChoose(shiftWindow(window, -1))}>Previous window</button>
      <button className="btn" onClick={() => onChoose(shiftWindow(window, 1))}>Next window</button>
      <button className="link" onClick={onRefresh}>Refresh readings</button>
    </div>
    <form className="perf-window-form" onSubmit={(e) => { e.preventDefault(); const selection = parseWindow(from, to); setError(!selection); if (selection) onChoose(selection); }}>
      <label>From <input type="datetime-local" step="1" value={from} onChange={(e) => setFrom(e.target.value)} required /></label>
      <label>To <input type="datetime-local" step="1" value={to} onChange={(e) => setTo(e.target.value)} required /></label>
      <button className="btn" type="submit">Open time window</button>
    </form>
    {error && <p role="alert" className="error">Choose an end time after the start time.</p>}
    <p className="small muted">Times use your local timezone. Explicit windows include all recorded boots at those wall-clock times. {fixed ? "Fixed window · does not move with time." : "Rolling window · follows current time."}
      {" "}Inspect an incident to open up to ten minutes from its start; step through longer incidents with Next window. Retention and result limits still apply.</p>
  </div>;
}

/** Why the machine is slow and what Nitely did about it (NIT-520), from the pace it measured
 * (NIT-519). Plain words and the numbers behind them; the guide to the cause never makes restarting
 * the answer and needs no administrator rights. */
function Pace({ pace, measuredMs, nonpaged, adjustments }: { pace: MachinePace | null | undefined; measuredMs: number | null | undefined; nonpaged: number | null | undefined; adjustments: PaceAdjustment[] }) {
  if (!pace) return null;
  const headline = paceHeadline(pace);
  const start = pace.start_ms == null ? "Unmeasured" : `${Math.round(pace.start_ms)} ms`;
  return (
    <section className="perf-section" aria-labelledby="perf-pace-title">
      <h2 id="perf-pace-title">How Nitely paces builds</h2>
      {headline ? (
        <div className="perf-pressure perf-pace-note" role="status">
          <p>{headline}</p>
        </div>
      ) : (
        <p className="muted">Programs start in {start} and memory is not short, so builds and tests use the machine's full share.</p>
      )}
      <div className="perf-metrics">
        <Metric label="Program start" value={start} detail={measuredMs ? `median of 5, ${timestamp(measuredMs)} · healthy is about ${HEALTHY_START_MS} ms` : `median of 5 · healthy is about ${HEALTHY_START_MS} ms`} />
        <Metric label="Kernel nonpaged" value={byteText(nonpaged)} detail="memory drivers hold, which no app owns" />
        <Metric
          label="Test threads per suite"
          value={pace.test_threads == null ? "Full share" : String(pace.test_threads)}
          detail={pace.suites == null ? "builds at once: every slot" : `${pace.suites} ${pace.suites === 1 ? "build" : "builds"} at once`}
        />
      </div>
      {adjustments.length > 0 && (
        <ul className="perf-pace-log" aria-label="What Nitely changed">
          {adjustments.map((a) => (
            <li key={a.wall_ms}>
              <span className="muted">{timestamp(a.wall_ms)}</span> {a.said.join(" ")}
            </li>
          ))}
        </ul>
      )}
      {(headline || adjustments.some((a) => a.pace.starts !== "normal" || a.pace.memory_short)) && (
        <details className="perf-pace-cause">
          <summary>Find the cause</summary>
          <p>
            Programs start slowly when something inspects each new program before it runs: security or antivirus software, endpoint monitoring, VPN or
            network filter drivers, and backup or sync tools that watch files. See whether the slowdown began when one of them was installed or updated,
            and look in its own settings or log for what it checks.
          </p>
          <p>
            Kernel nonpaged memory that keeps growing while no app grows is held by a driver. The tool that installed that driver is the likely cause, and
            updating it or reporting it to its maker is the lasting fix.
          </p>
          <p>Task Manager's Startup apps and Details tabs show what runs here, without administrator rights. Nitely keeps adapting in the meantime.</p>
        </details>
      )}
    </section>
  );
}

function Metric({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <div className="perf-metric">
      <span className="muted small">{label}</span>
      <strong>{value}</strong>
      <span className="muted small">{detail}</span>
    </div>
  );
}

function History({ data }: { data: PerformanceReport }) {
  const [metric, setMetric] = useState<ChartMetric>("cpu_percent");
  const chart = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(1000);
  useEffect(() => {
    const element = chart.current;
    if (!element) return;
    const resize = () => setWidth(Math.max(240, element.clientWidth));
    resize();
    const observer = new ResizeObserver(resize);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  const segments = chartSegments(data.points, data.coverage.gaps, metric);
  const extrema = chartSegments(data.points, data.coverage.gaps, metric, true);
  const values = data.points.flatMap((p) => [chartValue(p, metric), chartValue(p, metric, true)]).filter((v): v is number => v != null);
  const max = metric === "cpu_percent" ? 100 : Math.max(1, ...values);
  const [plotFrom, plotTo] = chartBounds(data.points, data.from_ms, data.to_ms);
  const x = (t: number) => 40 + ((width - 70) * (t - plotFrom)) / Math.max(1, plotTo - plotFrom);
  const y = (v: number) => 140 - (120 * v) / max;
  return (
    <div ref={chart}>
      <label className="perf-chart-choice">
        Counter{" "}
        <Select<ChartMetric>
          aria-label="Counter"
          value={metric}
          onChange={(v) => v && setMetric(v)}
          options={[
            { value: "cpu_percent", label: "CPU" },
            { value: "physical_available_bytes", label: "Available physical RAM" },
            { value: "commit_bytes", label: "Committed memory" },
            { value: "kernel_paged_bytes", label: "Kernel paged memory" },
            { value: "kernel_nonpaged_bytes", label: "Kernel nonpaged memory" },
          ]}
        />
      </label>
      <div className="perf-chart-legend">
        <span className="perf-legend-last">Last observed values</span>
        <span className="perf-legend-extreme">Observed {metric === "physical_available_bytes" ? "minima" : "peaks"} · not sustained</span>
        {data.coverage.boundary_points > 0 && <span className="perf-legend-boundary">Outside selected timestamps</span>}
      </div>
      {!values.length ? (
        <EmptyState title="No retained samples in this window.">Try another range or resume collection.</EmptyState>
      ) : (
        <svg
          className="perf-chart"
          viewBox={`0 0 ${width} 170`}
          role="img"
          aria-label={`${metric.replaceAll("_", " ")} history. Lines stop at collection gaps. Detail follows in the table.`}
        >
          {[20, 80, 140].map((v) => (
            <line key={v} x1="40" x2={width - 30} y1={v} y2={v} className="perf-grid" />
          ))}
          {[[plotFrom, data.from_ms], [data.to_ms, plotTo]].filter(([from, to]) => to > from).map(([from, to]) => (
            <rect key={from} className="perf-boundary" x={x(from)} y="20" width={x(to) - x(from)} height="120">
              <title>Outside selected range: {timestamp(from)} – {timestamp(to)}. Included in a full summary interval.</title>
            </rect>
          ))}
          {data.coverage.gaps.map((g, i) => (
            <rect
              key={i}
              className="perf-gap"
              x={x(Math.max(g.from_ms, data.from_ms))}
              y="20"
              width={Math.max(1, x(Math.min(g.to_ms, data.to_ms)) - x(Math.max(g.from_ms, data.from_ms)))}
              height="120"
            >
              <title>
                {g.kind.replaceAll("_", " ")}: {timestamp(g.from_ms)} – {timestamp(g.to_ms)}
              </title>
            </rect>
          ))}
          {segments.map((s, i) => (
            <g key={i}>
              <polyline className="perf-line" points={s.map((p) => `${x(p.to_ms)},${y(p.values[metric]!)}`).join(" ")} />
              {s.length === 1 && <circle className="perf-point" cx={x(s[0].to_ms)} cy={y(s[0].values[metric]!)} r="2" />}
            </g>
          ))}
          {extrema.map((s, i) => (
            <g key={i}>
              <polyline className="perf-line perf-extreme" points={s.map((p) => `${x(p.to_ms)},${y(chartValue(p, metric, true)!)}`).join(" ")} />
              {s.length === 1 && <circle className="perf-extreme-point" cx={x(s[0].to_ms)} cy={y(chartValue(s[0], metric, true)!)} r="2" />}
            </g>
          ))}
          <text x="40" y="13">
            {metric === "cpu_percent" ? "100%" : byteText(max)}
          </text>
          <text x="40" y="160">
            {new Date(plotFrom).toLocaleTimeString()}
          </text>
          <text x={width - 30} y="160" textAnchor="end">
            {new Date(plotTo).toLocaleTimeString()}
          </text>
        </svg>
      )}
      <p className="muted small">
        Blank bands are unrecorded time. Solid lines show last observed values; dashed lines preserve{" "}
        {metric === "physical_available_bytes" ? "minimum available RAM" : "observed peaks"} within each interval. Extrema do not establish sustained pressure.
        {data.coverage.boundary_points > 0 && " Shaded chart edges lie outside the exact selected range and belong to complete summary intervals."}
      </p>
      {!!data.points.length && (
        <details className="perf-details">
          <summary>Sample values and peaks</summary>
          <div className="perf-table-scroll">
            <table>
              <thead>
                <tr>
                  <th>Actual interval</th>
                  <th>Value</th>
                  <th>Observed {metric === "physical_available_bytes" ? "minimum" : "peak"}</th>
                  <th>Coverage</th>
                </tr>
              </thead>
              <tbody>
                {data.points.map((p, i) => (
                  <tr key={i}>
                    <td>{timestamp(p.from_ms)} – {timestamp(p.to_ms)}</td>
                    <td>{metric === "cpu_percent" ? numberText(p.values[metric], "%") : byteText(p.values[metric])}</td>
                    <td>
                      {metric === "cpu_percent"
                        ? numberText(p.peaks[metric], "%")
                        : byteText(metric === "physical_available_bytes" ? p.min_available_bytes : p.peaks[metric])}
                    </td>
                    <td>
                      {p.resolution} · {p.observations} samples{p.contains_gap ? " · contains gap" : ""}
                      {p.overlaps_range_boundary && " · full interval overlaps selected range; extrema may be outside it"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </details>
      )}
    </div>
  );
}
const HistoryChart = memo(History);

function Workloads({ data, fixed, now, onOpenIssue, onWindow }: { data: PerformanceReport; fixed: boolean; now: number; onOpenIssue: (key: string) => void; onWindow: (window: PerformanceWindow) => void }) {
  // Keep the chosen observation stable while the bounded live window moves on.
  const [selectedSample, setSelectedSample] = useState<AttributionSample | null>(null);
  const [sort, setSort] = useState<"cpu_percent" | "private_commit_bytes" | "process_read_bytes">("cpu_percent");
  const [error, setError] = useState<string | null>(null);
  const history = data.attribution ?? [];
  const sampleId = (s: AttributionSample) => `${s.session}/${s.monotonic_ms}`;
  const choices = selectedSample && !history.some((s) => sampleId(s) === sampleId(selectedSample)) ? [selectedSample, ...history] : history;
  const sample = selectedSample ?? (fixed ? windowAttribution(data) : data.attribution_latest);
  const context = (c: WorkloadContext) => (
    <>
      {c.project && <span>{c.project} · </span>}
      {c.issue && (
        <button className="link" onClick={() => onOpenIssue(c.issue!)}>
          {c.issue}
        </button>
      )}
      {c.run_id != null && (
        <>
          {" "}
          ·{" "}
          <button className="link" onClick={() => invoke("open_lane_log", { id: c.run_id }).catch((e) => setError(String(e)))}>
            run {c.run_id}
          </button>
        </>
      )}
      {c.job_id != null && <> · job {c.job_id}</>}
      {c.review_batch_id != null && <> · review batch {c.review_batch_id}</>}
      {c.worker_id != null && <> · worker {c.worker_id}</>}
      {c.lane != null && <> · lane {c.lane}</>}
    </>
  );
  return (
    <section className="perf-section">
      <div className="perf-heading">
        <h2>Workloads</h2>
        <label>
          Recorded at{" "}
          <Select
            aria-label="Workload sample"
            value={selectedSample ? sampleId(selectedSample) : "latest"}
            onChange={(v) => setSelectedSample(choices.find((s) => sampleId(s) === v) ?? null)}
            options={[
              { value: "latest", label: fixed ? "Last observation in this window" : "Latest sample" },
              ...choices.map((s) => ({ value: sampleId(s), label: timestamp(s.wall_ms), detail: s.resolution === "minute_peak" ? "Minute peaks" : undefined })),
            ]}
          />
        </label>
      </div>
      {error && <p className="error">{error}</p>}
      {fixed && <p className="small muted">Workloads and events use the fixed history window. Current machine readings above remain live.</p>}
      {sample && !fixed && !selectedSample && (now - sample.wall_ms > 15_000 || now < sample.wall_ms - 2000) && (
        <p className="muted">Attribution is stale or its clock changed. These values are the last recorded observation.</p>
      )}
      {selectedSample && selectedSample.wall_ms < data.from_ms && (
        <p className="muted small">The selected observation is now outside the rolling history window.</p>
      )}
      {data.coverage.attribution_truncated && (
        <p className="muted small">
          Showing the most recent {history.length} attribution observations in this window
          {data.coverage.attribution_first_returned_ms != null ? `, beginning ${timestamp(data.coverage.attribution_first_returned_ms)}` : ""}. Earlier retained
          observations are omitted; narrow the time range to inspect them.
          {data.coverage.attribution_first_returned_ms != null && data.coverage.attribution_first_returned_ms > data.from_ms && <>{" "}<button className="link"
            onClick={() => onWindow(earlierAttributionWindow(data, data.coverage.attribution_first_returned_ms!))}>Earlier observations</button></>}
        </p>
      )}
      {!sample || !sample.coverage.supported ? (
        <p className="muted">Process attribution is unavailable for this sample or platform. Machine counters remain separate.</p>
      ) : (
        <>
          <p className="small muted">
            {timestamp(sample.wall_ms)} · {sample.coverage.measured} of {sample.coverage.enumerated} processes measured · {sample.coverage.denied} inaccessible
            · {sample.coverage.partial_metrics} with partial counters{sample.coverage.truncated ? " · results truncated" : ""}
            {sample.coverage.first_sample ? " · first sample; interval counters unavailable" : ""}. Short-lived work may be missed between samples.
          </p>
          <div className="perf-attribution">
            {(
              [
                ["Nitely", sample.nitely],
                ["Related work", sample.related],
                ["Other processes", sample.other],
              ] as const
            ).map(([label, usage]) => (
              <Metric
                key={label}
                label={label}
                value={numberText(usage.cpu_percent, "% CPU")}
                detail={`${byteText(usage.private_commit_bytes)} private commit`}
              />
            ))}
            <Metric label="Unattributed CPU" value={numberText(sample.unattributed_cpu_percent, "%")} detail="Residual estimate; includes sampling skew" />
          </div>
          <p className="muted small">
            {sample.resolution === "minute_peak"
              ? "Minute peaks are separate maxima, not simultaneous or sustained usage. "
              : "Sampled CPU is normalized to the whole machine. "}
            Working sets overlap and cannot be added to machine RAM. Process I/O includes cached/network traffic and is not disk traffic.
          </p>
          <label className="perf-chart-choice">
            Sort workloads by{" "}
            <Select<typeof sort>
              aria-label="Sort workloads by"
              value={sort}
              onChange={(v) => v && setSort(v)}
              options={[
                { value: "cpu_percent", label: "CPU" },
                { value: "private_commit_bytes", label: "Private commit" },
                { value: "process_read_bytes", label: "Process reads" },
              ]}
            />
          </label>
          <div className="perf-table-scroll">
            <table>
              <thead>
                <tr>
                  <th>Workload</th>
                  <th>CPU</th>
                  <th>Private commit</th>
                  <th>Working sets</th>
                  <th>Process I/O</th>
                </tr>
              </thead>
              <tbody>
                {sortedWorkloads(sample.workloads, sort).map((w) => (
                  <tr key={w.id}>
                    <td>
                      <strong>{w.kind.replaceAll("_", " ")}</strong>
                      <div className="small">{context(w.context)}</div>
                    </td>
                    <UsageCells usage={w.usage} sample={sample} />
                  </tr>
                ))}
                {!sample.workloads.length && (
                  <tr>
                    <td colSpan={5}>No attributed Nitely workloads in this sample.</td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
          <details className="perf-details">
            <summary>Top other processes · {sample.top_other.length} shown</summary>
            <div className="perf-table-scroll">
              <table>
                <thead>
                  <tr>
                    <th>Executable</th>
                    <th>CPU</th>
                    <th>Private commit</th>
                    <th>Working sets</th>
                    <th>Process I/O</th>
                  </tr>
                </thead>
                <tbody>
                  {sample.top_other.map((p) => (
                    <tr key={`${p.identity.boot}/${p.identity.pid}/${p.identity.started}`}>
                      <td>
                        {p.basename}
                        <div className="small muted">PID {p.identity.pid}</div>
                      </td>
                      <UsageCells usage={p.usage} sample={sample} />
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </details>
        </>
      )}
      <details className="perf-details">
        <summary>
          Registered work in this window · {data.workloads.length}
          {data.coverage.workloads_truncated ? " shown (bounded)" : " records"}
        </summary>
        <p className="muted small">
          Launch/exit records include work that finishes between samples. Job totals below include descendants and are separate from sampled process totals; do
          not add them together. A missing exit time means exit was not recorded.
        </p>
        <div className="perf-table-scroll">
          <table>
            <thead>
              <tr>
                <th>Work</th>
                <th>Recorded lifetime</th>
                <th>Job CPU time</th>
                <th>Job peak private commit</th>
                <th>Job process I/O</th>
              </tr>
            </thead>
            <tbody>
              {data.workloads.map((w) => (
                <tr key={w.id}>
                  <td>
                    {w.kind.replaceAll("_", " ")}
                    <div className="small">{context(w.context)}</div>
                  </td>
                  <td>
                    {timestamp(w.start_ms)}
                    <div>{w.end_ms == null ? "Exit not recorded" : `${timestamp(w.end_ms)}${w.end_observed ? " (observed later)" : ""}`}</div>
                  </td>
                  <td>{w.job_accounting ? numberText(w.job_accounting.cpu_100ns / 10_000_000, " CPU seconds") : "Unavailable"}</td>
                  <td>{byteText(w.job_accounting?.peak_private_commit_bytes)}</td>
                  <td>
                    {w.job_accounting ? `${byteText(w.job_accounting.read_bytes)} read · ${byteText(w.job_accounting.write_bytes)} written` : "Unavailable"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </section>
  );
}
function UsageCells({ usage: u, sample }: { usage: PerformanceUsage; sample: AttributionSample }) {
  return (
    <>
      <td>{numberText(u.cpu_percent, "%")}</td>
      <td>{byteText(u.private_commit_bytes)}</td>
      <td>{byteText(u.working_set_sum_bytes)}</td>
      <td>
        {byteText(u.process_read_bytes)} read · {byteText(u.process_write_bytes)} written
        <div className="small muted">{sample.interval_ms == null ? "Interval unavailable" : `over ${numberText(sample.interval_ms / 1000, "s")}`}</div>
      </td>
    </>
  );
}

function RecorderSettings({ settings, onChanged }: { settings: PerformanceSettings; onChanged: () => void }) {
  const [hours, setHours] = useState(settings.detailed_hours);
  const [days, setDays] = useState(settings.summary_days);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const save = async (enabled: boolean) => {
    setBusy(true);
    setMessage(null);
    try {
      await invoke("set_performance_settings", { settings: { enabled, detailed_hours: hours, summary_days: days } });
      onChanged();
      setMessage(enabled ? "Collection enabled. Settings saved." : "Collection paused. In-flight samples may finish before the recorder releases its lock.");
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    setBusy(true);
    setMessage(null);
    try {
      await invoke("clear_performance_history");
      setConfirm(false);
      onChanged();
      setMessage("Recorded history cleared.");
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="perf-section">
      <h2>Recording & retention</h2>
      <p className="muted small">
        History stays on this machine. Pausing collection does not pause agents or builds. Clearing history removes recorded samples, workload observations and
        events.
      </p>
      {isLive && <p className="muted small">Live browser preview is read-only. Change recording in the desktop app.</p>}
      <fieldset disabled={busy || isLive} className="perf-controls">
        <label>
          Detailed history{" "}
          <Select
            aria-label="Detailed history"
            value={String(hours)}
            onChange={(v) => v && setHours(+v)}
            disabled={busy || isLive}
            options={[1, 6, 12, 24, hours]
              .filter((v, i, a) => a.indexOf(v) === i)
              .sort((a, b) => a - b)
              .map((n) => ({ value: String(n), label: `${n} hours` }))}
          />
        </label>
        <label>
          Minute summaries{" "}
          <Select
            aria-label="Minute summaries"
            value={String(days)}
            onChange={(v) => v && setDays(+v)}
            disabled={busy || isLive}
            options={[1, 7, 14, 30, days]
              .filter((v, i, a) => a.indexOf(v) === i)
              .sort((a, b) => a - b)
              .map((n) => ({ value: String(n), label: `${n} days` }))}
          />
        </label>
        <button className="btn" onClick={() => save(settings.enabled)}>
          Save retention
        </button>
        <button className="btn" onClick={() => save(!settings.enabled)}>
          {settings.enabled ? "Pause collection" : "Resume collection"}
        </button>
        <button
          className="btn"
          disabled={settings.enabled}
          title={settings.enabled ? "Pause collection before clearing history" : undefined}
          onClick={() => setConfirm(true)}
        >
          Clear history…
        </button>
      </fieldset>
      {confirm && (
        <div className="perf-confirm">
          <p>Clear all recorded performance history on this machine? This cannot be undone.</p>
          <button className="btn" disabled={busy || isLive || settings.enabled} onClick={clear}>
            Clear recorded history
          </button>{" "}
          <button className="btn" disabled={busy} onClick={() => setConfirm(false)}>
            Cancel
          </button>
        </div>
      )}
      {message && (
        <p role="status" className="small">
          {message}
        </p>
      )}
    </section>
  );
}
