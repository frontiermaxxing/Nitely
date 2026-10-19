import { memo, useEffect, useState } from "react";
import { invoke, isLive } from "./backend";
import { Loading, EmptyState } from "./ui";
import { useLanes } from "./LanesStrip";
import { byteText, chartSegments, currentPressure, freshness, numberText, performanceQuery, sortedWorkloads, timestamp } from "./performanceModel";
import type { AttributionSample, ChartMetric, PerformanceRange, PerformanceReport, PerformanceSettings, PerformanceUsage, WorkloadContext } from "./performanceModel";
import "./performance.css";

const RANGES: PerformanceRange[] = ["15m", "1h", "6h", "24h", "7d", "restart"];
const RANGE_LABEL = (r: PerformanceRange) => r === "restart" ? "Before last restart" : r;

/** Reads the recorder's cache; opening this page never creates another collector. */
export function Performance(props: { version: number; onOpenIssue: (key: string) => void; onSettings: () => void }) {
  const [range, setRange] = useState<PerformanceRange>("1h");
  const [history, setHistory] = useState<PerformanceReport | null>(null);
  const [current, setCurrent] = useState<PerformanceReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [currentError, setCurrentError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [now, setNow] = useState(Date.now());
  const [dismissed, setDismissed] = useState(false);
  const lanes = useLanes(props.version);
  useEffect(() => {
    let live = true; let running = false; let timer: ReturnType<typeof setTimeout>;
    const read = async () => {
      if (running) return;
      running = true;
      try {
        const report = await invoke<PerformanceReport>("performance_query", { query: performanceQuery(range, Date.now()) });
        if (live) { setHistory(report); setError(null); }
      } catch (e) { if (live) setError(String(e)); }
      running = false;
      // Historical queries are bounded but cost more than Now. Fixed restart windows read once.
      if (live && range !== "restart") timer = setTimeout(() => { if (!document.hidden) void read(); }, 60_000);
    };
    const visible = () => { if (!document.hidden) { clearTimeout(timer); void read(); } };
    void read(); document.addEventListener("visibilitychange", visible);
    return () => { live = false; clearTimeout(timer); document.removeEventListener("visibilitychange", visible); };
  }, [range, props.version, reload]);
  useEffect(() => {
    let live = true; let running = false; let timer: ReturnType<typeof setTimeout>;
    const read = async () => {
      if (running) return;
      running = true;
      try { const report = await invoke<PerformanceReport>("performance_query", { query: { view: "now", limit: 200 } }); if (live) { setCurrent(report); setCurrentError(null); } }
      catch (e) { if (live) setCurrentError(String(e)); }
      running = false;
      if (live) timer = setTimeout(() => { if (!document.hidden) void read(); }, 5000);
    };
    const visible = () => { if (!document.hidden) { clearTimeout(timer); void read(); } };
    void read(); document.addEventListener("visibilitychange", visible);
    const clock = setInterval(() => setNow(Date.now()), 1000);
    return () => { live = false; clearTimeout(timer); clearInterval(clock); document.removeEventListener("visibilitychange", visible); };
  }, [props.version, reload]);
  const data = history ? { ...history, ...(current ? { latest: current.latest, settings: current.settings, status: current.status, admission_limits: current.admission_limits, attribution_latest: current.attribution_latest, attribution_age_ms: current.attribution_age_ms } : {}) } : current;
  const chooseRange = (r: PerformanceRange) => { if (r !== range) { setRange(r); setHistory(null); setError(null); } };
  const metrics = data?.latest?.metrics;
  const pressure = current && currentPressure(current, now);
  useEffect(() => { if (current && !currentPressure(current, Date.now())) setDismissed(false); }, [current]);
  return <div className="page perf" data-testid="performance-page">
    <div className="page-head"><h1>Performance</h1><span className="head-glance">This machine · all projects</span></div>
    <p className="perf-intro">Machine pressure, the work using it, and what happened before a slowdown.</p>
    {error && <p className="error" role="alert">{error} <button className="link" onClick={() => setReload(n => n + 1)}>Try again</button></p>}
    {currentError && currentError !== error && <p className="error" role="alert">Current readings: {currentError}</p>}
    {!data && !error && <Loading />}
    {data && <>
      <div className="perf-freshness"><span className={data.settings.enabled && data.latest && now - data.latest.wall_ms >= -2000 && now - data.latest.wall_ms <= 15_000 ? "live-ink" : "muted"}>{freshness(data, now)}</span><span className="muted" title={data.latest ? timestamp(data.latest.wall_ms) : undefined}>{data.latest ? `Last sample ${timestamp(data.latest.wall_ms)}` : "Collection may take a few seconds to start."}</span></div>
      {pressure && !dismissed && <div className="perf-pressure" role="status"><div><strong>Sustained pressure observed</strong><p>{pressure.observed.map(s => s.replaceAll("_", " ")).join(" · ")} since {timestamp(pressure.from_ms)}. Workload overlap can help investigate; it does not establish the cause.</p></div><button className="link" onClick={() => setDismissed(true)}>Dismiss</button></div>}
      <div className="perf-metrics">
        <Metric label="CPU" value={numberText(metrics?.cpu_percent, "%")} detail="Whole machine" />
        <Metric label="Available RAM" value={byteText(metrics?.physical_available_bytes)} detail={`of ${byteText(metrics?.physical_total_bytes)} physical RAM`} />
        <Metric label="Committed memory" value={byteText(metrics?.commit_bytes)} detail={`limit ${byteText(metrics?.commit_limit_bytes)}`} />
        <Metric label="Paging" value={numberText(metrics?.pages_input_per_sec, " pages/s")} detail={`out ${numberText(metrics?.pages_output_per_sec, " pages/s")}`} />
        <Metric label="Disk activity" value={metrics?.disk_read_bytes_per_sec == null ? "Unavailable" : `${byteText(metrics.disk_read_bytes_per_sec)}/s read`} detail={metrics?.disk_write_bytes_per_sec == null ? "Write rate unavailable" : `${byteText(metrics.disk_write_bytes_per_sec)}/s write`} />
        <Metric label="Disk pressure" value={metrics?.disk_latency_seconds == null ? "Unavailable" : numberText(metrics.disk_latency_seconds * 1000, " ms latency")} detail={`queue ${numberText(metrics?.disk_queue)}`} />
      </div>
      <p className="muted small">Counters are measured when supported. Disk activity is transfer traffic, not free disk space. Kernel memory: {byteText(metrics?.kernel_paged_bytes)} paged · {byteText(metrics?.kernel_nonpaged_bytes)} nonpaged.</p>
    </>}
    <section className="perf-section"><div className="perf-heading"><h2>History</h2><div className="perf-ranges" role="group" aria-label="Performance time range">{RANGES.map(r => <button key={r} aria-pressed={range === r} onClick={() => chooseRange(r)}>{RANGE_LABEL(r)}</button>)}</div></div>
      {!history && !error && <Loading />}
      {history && <><HistoryChart data={history} /><p className="muted small">{timestamp(history.from_ms)} – {timestamp(history.to_ms)} · {history.coverage.returned_points} of {history.coverage.source_points} points · {history.coverage.resolution.replaceAll("_", " ")}</p>
        {range === "restart" && <p className="small">Ends at the last retained sample of the previous boot. The exact shutdown time is unknown.</p>}
        <details className="perf-details"><summary>Collection coverage · {history.coverage.gaps.length} recorded gaps</summary><ul>{history.coverage.gaps.map((g, i) => <li key={i}>{g.kind.replaceAll("_", " ")} · {timestamp(g.from_ms)} – {timestamp(g.to_ms)}</li>)}</ul>{history.coverage.notes.map(n => <p className="muted small" key={n}>{n}</p>)}{(history.coverage.bounded || history.coverage.gaps_truncated || history.coverage.sessions_truncated || history.coverage.events_truncated) && <p>Results are bounded; some detail is omitted. Narrow the time range to inspect it.</p>}</details>
      </>}
    </section>
    {data && <>
      <Workloads key={`${range}/${reload}`} data={data} now={now} onOpenIssue={props.onOpenIssue} />
      <section className="perf-section"><h2>Pressure episodes & workload events</h2>
        {!data.episodes.length && <p className="muted">No sustained pressure established in the retained detailed samples for this range.</p>}
        {data.episodes.map(e => <div className="perf-episode" key={e.from_ms}><strong>{e.observed.map(s => s.replaceAll("_", " ")).join(" · ")}</strong><span>{timestamp(e.from_ms)} – {timestamp(e.to_ms)}</span><span className="muted">{e.observations} observations{e.inferred_causes.length ? ` · possible causes: ${e.inferred_causes.join(", ")}` : " · cause not established"}</span></div>)}
        <details className="perf-details"><summary>{data.events.length} workload events in this window</summary>{data.events.length ? <ul>{data.events.map((e, i) => <li key={i}>{timestamp(e.wall_ms)} · {e.kind.replaceAll("_", " ")} · {e.project} · <button className="link" onClick={() => invoke("open_lane_log", { id: e.run }).catch(err => setError(String(err)))}>run {e.run}</button>{e.issue && <> · <button className="link" onClick={() => props.onOpenIssue(e.issue!)}>{e.issue}</button></>}</li>)}</ul> : <p className="muted">No recorded workload transitions in this window.</p>}</details>
      </section>
      <section className="perf-section"><div className="perf-heading"><h2>Admission settings</h2><button className="link" onClick={props.onSettings}>Open build lane settings</button></div><p>{lanes?.slots ? `${lanes.slots.used} of ${lanes.slots.total} build slots in use. ` : "Build slot usage unavailable. "}Memory floor {byteText(data.admission_limits?.floor)} · per-run cap {byteText(data.admission_limits?.cap)}.</p><p className="muted small">Current settings, using the recorder's cached memory total. New-work admission checks live memory separately. This page does not stop work or change concurrency.</p>{lanes?.waiting_for_memory && <p>{lanes.waiting_for_memory}</p>}</section>
      <RecorderSettings settings={data.settings} onChanged={() => setReload(n => n + 1)} />
    </>}
  </div>;
}

function Metric({ label, value, detail }: { label: string; value: string; detail: string }) {
  return <div className="perf-metric"><span className="muted small">{label}</span><strong>{value}</strong><span className="muted small">{detail}</span></div>;
}

function History({ data }: { data: PerformanceReport }) {
  const [metric, setMetric] = useState<ChartMetric>("cpu_percent");
  const segments = chartSegments(data.points, data.coverage.gaps, metric);
  const values = data.points.flatMap(p => p.values[metric] == null ? [] : [p.values[metric]!]);
  const max = metric === "cpu_percent" ? 100 : Math.max(1, ...values);
  const x = (t: number) => 40 + 930 * (t - data.from_ms) / Math.max(1, data.to_ms - data.from_ms);
  const y = (v: number) => 140 - 120 * v / max;
  return <><label className="perf-chart-choice">Counter <select value={metric} onChange={e => setMetric(e.target.value as ChartMetric)}><option value="cpu_percent">CPU</option><option value="physical_available_bytes">Available physical RAM</option><option value="commit_bytes">Committed memory</option></select></label>
    {!values.length ? <EmptyState title="No retained samples in this window.">Try another range or resume collection.</EmptyState> : <svg className="perf-chart" viewBox="0 0 1000 170" role="img" aria-label={`${metric.replaceAll("_", " ")} history. Lines stop at collection gaps. Detail follows in the table.`}>
      {[20, 80, 140].map(v => <line key={v} x1="40" x2="970" y1={v} y2={v} className="perf-grid" />)}
      {data.coverage.gaps.map((g, i) => <rect key={i} className="perf-gap" x={x(Math.max(g.from_ms, data.from_ms))} y="20" width={Math.max(1, x(Math.min(g.to_ms, data.to_ms)) - x(Math.max(g.from_ms, data.from_ms)))} height="120"><title>{g.kind.replaceAll("_", " ")}: {timestamp(g.from_ms)} – {timestamp(g.to_ms)}</title></rect>)}
      {segments.map((s, i) => <g key={i}><polyline className="perf-line" points={s.map(p => `${x(p.to_ms)},${y(p.values[metric]!)}`).join(" ")} />{s.length === 1 && <circle className="perf-point" cx={x(s[0].to_ms)} cy={y(s[0].values[metric]!)} r="2" />}</g>)}
      <text x="40" y="13">{metric === "cpu_percent" ? "100%" : byteText(max)}</text><text x="40" y="160">{new Date(data.from_ms).toLocaleTimeString()}</text><text x="970" y="160" textAnchor="end">{new Date(data.to_ms).toLocaleTimeString()}</text>
    </svg>}
    <p className="muted small">Blank bands are unrecorded time. Lines show recorded values or bucket averages; they do not turn peaks into sustained pressure.</p>
    {!!data.points.length && <details className="perf-details"><summary>Sample values and peaks</summary><div className="perf-table-scroll"><table><thead><tr><th>Through</th><th>Value</th><th>Observed {metric === "physical_available_bytes" ? "minimum" : "peak"}</th><th>Coverage</th></tr></thead><tbody>{data.points.map((p, i) => <tr key={i}><td>{timestamp(p.to_ms)}</td><td>{metric === "cpu_percent" ? numberText(p.values[metric], "%") : byteText(p.values[metric])}</td><td>{metric === "cpu_percent" ? numberText(p.peaks[metric], "%") : byteText(metric === "physical_available_bytes" ? p.min_available_bytes : p.peaks[metric])}</td><td>{p.resolution} · {p.observations} samples{p.contains_gap ? " · contains gap" : ""}</td></tr>)}</tbody></table></div></details>}
  </>;
}
const HistoryChart = memo(History);

function Workloads({ data, now, onOpenIssue }: { data: PerformanceReport; now: number; onOpenIssue: (key: string) => void }) {
  // Keep the chosen observation stable while the bounded live window moves on.
  const [selectedSample, setSelectedSample] = useState<AttributionSample | null>(null);
  const [sort, setSort] = useState<"cpu_percent" | "private_commit_bytes" | "process_read_bytes">("cpu_percent");
  const [error, setError] = useState<string | null>(null);
  const history = data.attribution ?? [];
  const sampleId = (s: AttributionSample) => `${s.session}/${s.monotonic_ms}`;
  const choices = selectedSample && !history.some(s => sampleId(s) === sampleId(selectedSample)) ? [selectedSample, ...history] : history;
  const sample = selectedSample ?? data.attribution_latest;
  const context = (c: WorkloadContext) => <>{c.project && <span>{c.project} · </span>}{c.issue && <button className="link" onClick={() => onOpenIssue(c.issue!)}>{c.issue}</button>}{c.run_id != null && <> · <button className="link" onClick={() => invoke("open_lane_log", { id: c.run_id }).catch(e => setError(String(e)))}>run {c.run_id}</button></>}{c.job_id != null && <> · job {c.job_id}</>}{c.review_batch_id != null && <> · review batch {c.review_batch_id}</>}{c.worker_id != null && <> · worker {c.worker_id}</>}{c.lane != null && <> · lane {c.lane}</>}</>;
  return <section className="perf-section"><div className="perf-heading"><h2>Workloads</h2><label>Recorded at <select aria-label="Workload sample" value={selectedSample ? sampleId(selectedSample) : "latest"} onChange={e => setSelectedSample(choices.find(s => sampleId(s) === e.target.value) ?? null)}><option value="latest">Latest sample</option>{choices.map(s => <option key={sampleId(s)} value={sampleId(s)}>{timestamp(s.wall_ms)}{s.resolution === "minute_peak" ? " · minute peaks" : ""}</option>)}</select></label></div>
    {error && <p className="error">{error}</p>}
    {sample && !selectedSample && (now - sample.wall_ms > 15_000 || now < sample.wall_ms - 2000) && <p className="muted">Attribution is stale or its clock changed. These values are the last recorded observation.</p>}
    {selectedSample && selectedSample.wall_ms < data.from_ms && <p className="muted small">The selected observation is now outside the rolling history window.</p>}
    {data.coverage.attribution_truncated && <p className="muted small">Showing the most recent {history.length} attribution observations in this window{data.coverage.attribution_first_returned_ms != null ? `, beginning ${timestamp(data.coverage.attribution_first_returned_ms)}` : ""}. Earlier retained observations are omitted; narrow the time range to inspect them.</p>}
    {!sample || !sample.coverage.supported ? <p className="muted">Process attribution is unavailable for this sample or platform. Machine counters remain separate.</p> : <>
      <p className="small muted">{timestamp(sample.wall_ms)} · {sample.coverage.measured} of {sample.coverage.enumerated} processes measured · {sample.coverage.denied} inaccessible · {sample.coverage.partial_metrics} with partial counters{sample.coverage.truncated ? " · results truncated" : ""}{sample.coverage.first_sample ? " · first sample; interval counters unavailable" : ""}. Short-lived work may be missed between samples.</p>
      <div className="perf-attribution">{([['Nitely', sample.nitely], ['Related work', sample.related], ['Other processes', sample.other]] as const).map(([label, usage]) => <Metric key={label} label={label} value={numberText(usage.cpu_percent, "% CPU")} detail={`${byteText(usage.private_commit_bytes)} private commit`} />)}<Metric label="Unattributed CPU" value={numberText(sample.unattributed_cpu_percent, "%")} detail="Residual estimate; includes sampling skew" /></div>
      <p className="muted small">{sample.resolution === "minute_peak" ? "Minute peaks are separate maxima, not simultaneous or sustained usage. " : "Sampled CPU is normalized to the whole machine. "}Working sets overlap and cannot be added to machine RAM. Process I/O includes cached/network traffic and is not disk traffic.</p>
      <label className="perf-chart-choice">Sort workloads by <select value={sort} onChange={e => setSort(e.target.value as typeof sort)}><option value="cpu_percent">CPU</option><option value="private_commit_bytes">Private commit</option><option value="process_read_bytes">Process reads</option></select></label>
      <div className="perf-table-scroll"><table><thead><tr><th>Workload</th><th>CPU</th><th>Private commit</th><th>Working sets</th><th>Process I/O</th></tr></thead><tbody>{sortedWorkloads(sample.workloads, sort).map(w => <tr key={w.id}><td><strong>{w.kind.replaceAll("_", " ")}</strong><div className="small">{context(w.context)}</div></td><UsageCells usage={w.usage} sample={sample} /></tr>)}{!sample.workloads.length && <tr><td colSpan={5}>No attributed Nitely workloads in this sample.</td></tr>}</tbody></table></div>
      <details className="perf-details"><summary>Top other processes · {sample.top_other.length} shown</summary><div className="perf-table-scroll"><table><thead><tr><th>Executable</th><th>CPU</th><th>Private commit</th><th>Working sets</th><th>Process I/O</th></tr></thead><tbody>{sample.top_other.map(p => <tr key={`${p.identity.boot}/${p.identity.pid}/${p.identity.started}`}><td>{p.basename}<div className="small muted">PID {p.identity.pid}</div></td><UsageCells usage={p.usage} sample={sample} /></tr>)}</tbody></table></div></details>
    </>}
    <details className="perf-details"><summary>Registered work in this window · {data.workloads.length}{data.coverage.workloads_truncated ? " shown (bounded)" : " records"}</summary><p className="muted small">Launch/exit records include work that finishes between samples. Job totals below include descendants and are separate from sampled process totals; do not add them together. A missing exit time means exit was not recorded.</p><div className="perf-table-scroll"><table><thead><tr><th>Work</th><th>Recorded lifetime</th><th>Job CPU time</th><th>Job peak private commit</th><th>Job process I/O</th></tr></thead><tbody>{data.workloads.map(w => <tr key={w.id}><td>{w.kind.replaceAll("_", " ")}<div className="small">{context(w.context)}</div></td><td>{timestamp(w.start_ms)}<div>{w.end_ms == null ? "Exit not recorded" : `${timestamp(w.end_ms)}${w.end_observed ? " (observed later)" : ""}`}</div></td><td>{w.job_accounting ? numberText(w.job_accounting.cpu_100ns / 10_000_000, " CPU seconds") : "Unavailable"}</td><td>{byteText(w.job_accounting?.peak_private_commit_bytes)}</td><td>{w.job_accounting ? `${byteText(w.job_accounting.read_bytes)} read · ${byteText(w.job_accounting.write_bytes)} written` : "Unavailable"}</td></tr>)}</tbody></table></div></details>
  </section>;
}
function UsageCells({ usage: u, sample }: { usage: PerformanceUsage; sample: AttributionSample }) {
  return <><td>{numberText(u.cpu_percent, "%")}</td><td>{byteText(u.private_commit_bytes)}</td><td>{byteText(u.working_set_sum_bytes)}</td><td>{byteText(u.process_read_bytes)} read · {byteText(u.process_write_bytes)} written<div className="small muted">{sample.interval_ms == null ? "Interval unavailable" : `over ${numberText(sample.interval_ms / 1000, "s")}`}</div></td></>;
}

function RecorderSettings({ settings, onChanged }: { settings: PerformanceSettings; onChanged: () => void }) {
  const [hours, setHours] = useState(settings.detailed_hours);
  const [days, setDays] = useState(settings.summary_days);
  const [busy, setBusy] = useState(false); const [confirm, setConfirm] = useState(false); const [message, setMessage] = useState<string | null>(null);
  const save = async (enabled: boolean) => { setBusy(true); setMessage(null); try { await invoke("set_performance_settings", { settings: { enabled, detailed_hours: hours, summary_days: days } }); onChanged(); setMessage(enabled ? "Collection enabled. Settings saved." : "Collection paused. In-flight samples may finish before the recorder releases its lock."); } catch (e) { setMessage(String(e)); } finally { setBusy(false); } };
  const clear = async () => { setBusy(true); setMessage(null); try { await invoke("clear_performance_history"); setConfirm(false); onChanged(); setMessage("Recorded history cleared."); } catch (e) { setMessage(String(e)); } finally { setBusy(false); } };
  return <section className="perf-section"><h2>Recording & retention</h2><p className="muted small">History stays on this machine. Pausing collection does not pause agents or builds. Clearing history removes recorded samples, workload observations and events.</p>{isLive && <p className="muted small">Live browser preview is read-only. Change recording in the desktop app.</p>}
    <fieldset disabled={busy || isLive} className="perf-controls"><label>Detailed history <select value={hours} onChange={e => setHours(+e.target.value)}>{[1, 6, 12, 24, hours].filter((v, i, a) => a.indexOf(v) === i).sort((a, b) => a - b).map(n => <option key={n} value={n}>{n} hours</option>)}</select></label><label>Minute summaries <select value={days} onChange={e => setDays(+e.target.value)}>{[1, 7, 14, 30, days].filter((v, i, a) => a.indexOf(v) === i).sort((a, b) => a - b).map(n => <option key={n} value={n}>{n} days</option>)}</select></label><button className="btn" onClick={() => save(settings.enabled)}>Save retention</button><button className="btn" onClick={() => save(!settings.enabled)}>{settings.enabled ? "Pause collection" : "Resume collection"}</button><button className="btn" disabled={settings.enabled} title={settings.enabled ? "Pause collection before clearing history" : undefined} onClick={() => setConfirm(true)}>Clear history…</button></fieldset>
    {confirm && <div className="perf-confirm"><p>Clear all recorded performance history on this machine? This cannot be undone.</p><button className="btn" disabled={busy || isLive || settings.enabled} onClick={clear}>Clear recorded history</button> <button className="btn" disabled={busy} onClick={() => setConfirm(false)}>Cancel</button></div>}{message && <p role="status" className="small">{message}</p>}
  </section>;
}
