import { useEffect, useMemo, useState } from "react";
import { invoke } from "./backend";
import type { Activity as ActivityData, ActivityLane, ActivityMark, ActivitySegment, IssueRecord, MarkKind, Project, SegmentState } from "./types";
import { AgentMark, EmptyState, EVENT_LABEL, EventMark, IssueToken, StateDot, VENDOR_LABEL, livenessFrom, vendorOf, type Liveness } from "./ui";

const WINDOWS = [1, 6, 12, 24] as const;
type Hours = (typeof WINDOWS)[number];
const DEFAULT_HOURS: Hours = 12;
/** Ticks on the time axis per window, in minutes. */
const TICK_MIN: Record<Hours, number> = { 1: 10, 6: 60, 12: 120, 24: 180 };
/** Columns in the aggregate band. */
const BUCKETS = 144;
const REFETCH_MS = 60000;

const SEGMENT_LABEL: Record<SegmentState, string> = {
  working: "working",
  waiting: "waiting for you",
  idle: "idle",
  stale: "nothing recorded",
  seen: "active (state not recorded yet)",
};

const LEGEND: MarkKind[] = ["start", "commit", "gate_pass", "gate_fail", "review_pass", "review_changes", "land", "land_fail", "ask", "handoff"];

/** What every agent session did over the last hours: one lane each, grouped by project. */
export function Activity(props: { now: number; version: number; projects: Project[]; onOpenIssue: (key: string) => void }) {
  const [hours, setHours] = useState<Hours>(initialHours);
  const [data, setData] = useState<ActivityData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), REFETCH_MS);
    return () => clearInterval(id);
  }, []);
  useEffect(() => {
    let live = true;
    invoke<ActivityData>("activity", { hours }).then(
      (d) => live && (setData(d), setError(null)),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [hours, props.version, tick]);

  const issues = useMemo(() => new Map((data?.issues ?? []).map((i) => [i.key, i])), [data]);
  const groups = useMemo(() => groupLanes(data?.lanes ?? [], props.projects), [data, props.projects]);
  const working = data?.lanes.filter((l) => liveness(l, props.now) === "working").length ?? 0;
  const waiting = data?.lanes.filter((l) => liveness(l, props.now) === "waiting").length ?? 0;

  return (
    <div className="page act">
      <div className="page-head">
        <h1>Activity</h1>
        {data && (
          <div className="head-glance">
            {waiting > 0 && (
              <span className="attention-ink">
                <StateDot state="waiting" /> {waiting} waiting for you
              </span>
            )}
            <span>
              <StateDot state="working" /> {working} working now
            </span>
            <span>
              {data.lanes.length} session{data.lanes.length === 1 ? "" : "s"} in {hours}h
            </span>
          </div>
        )}
        <div className="act-window" role="group" aria-label="Time window">
          {WINDOWS.map((h) => (
            <button key={h} className={h === hours ? "on" : undefined} aria-pressed={h === hours} onClick={() => setHours(h)}>
              {h}h
            </button>
          ))}
        </div>
      </div>
      {error && <p className="error">{error}</p>}
      {data && data.lanes.length === 0 && (
        <EmptyState title={`No agent ran in the last ${hours} hour${hours === 1 ? "" : "s"}.`}>
          Sessions show up here as soon as an agent with Nitely's hooks starts working.
        </EmptyState>
      )}
      {data && data.lanes.length > 0 && (
        <div className="act-chart" style={{ ["--unrecorded" as string]: unrecordedPct(data) }}>
          <Axis data={data} hours={hours} />
          <Band data={data} now={props.now} />
          {groups.map((g) => (
            <section key={g.key} className="act-group">
              <h2>{g.name}</h2>
              {g.lanes.map((lane) => (
                <Lane key={lane.session_id} lane={lane} data={data} now={props.now} issues={issues} onOpenIssue={props.onOpenIssue} />
              ))}
            </section>
          ))}
          <Legend data={data} />
        </div>
      )}
    </div>
  );
}

function Lane(props: { lane: ActivityLane; data: ActivityData; now: number; issues: Map<string, IssueRecord>; onOpenIssue: (key: string) => void }) {
  const { lane, data } = props;
  const state = liveness(lane, props.now);
  const recent = lane.issues.slice(-2).reverse();
  const more = lane.issues.length - recent.length;
  const vendor = VENDOR_LABEL[vendorOf(lane.vendor)];
  return (
    <div className={`act-row${state === "ended" || state === "stale" ? " act-quiet" : ""}`}>
      <div className="act-who" title={`${vendor} in ${lane.cwd}\nstarted ${clock(lane.started_at)}, last seen ${clock(lane.last_seen_at)}`}>
        <AgentMark vendor={lane.vendor} state={state} />
        <div className="act-who-text">
          <div className="act-issues">
            {recent.length === 0 && <span className="act-place mono">{lane.place}</span>}
            {recent.map((k) => {
              const issue = props.issues.get(k);
              return issue ? <IssueToken key={k} issue={issue} onOpen={props.onOpenIssue} /> : <span key={k} className="mono small muted">{k}</span>;
            })}
            {more > 0 && <span className="act-more" title={lane.issues.slice(0, more).join(", ")}>+{more}</span>}
          </div>
          {recent.length > 0 && <div className="act-sub">{props.issues.get(recent[0])?.title ?? lane.place}</div>}
        </div>
      </div>
      <div className="act-track">
        <Gridlines data={data} />
        <span className="act-base" />
        {lane.segments.map((s) => (
          <span
            key={`${s.from}-${s.state}`}
            className={`act-seg act-seg-${s.state}`}
            style={span(data, s.from, s.to)}
            title={`${SEGMENT_LABEL[s.state]} ${clock(s.from)} to ${clock(s.to)} (${duration(s.to - s.from)})`}
          />
        ))}
        {lane.marks.map((m, i) => (
          <Mark key={`${m.ts}-${m.kind}-${i}`} mark={m} data={data} onOpenIssue={props.onOpenIssue} />
        ))}
      </div>
    </div>
  );
}

function Mark(props: { mark: ActivityMark; data: ActivityData; onOpenIssue: (key: string) => void }) {
  const { mark } = props;
  const what = EVENT_LABEL[mark.kind];
  const detail = mark.kind === "commit" || !mark.summary ? "" : `\n${mark.summary}`;
  const title = `${what}${mark.issue_key ? ` · ${mark.issue_key}` : ""} · ${clock(mark.ts)}${detail}`;
  const key = mark.issue_key;
  return (
    <button
      type="button"
      className={`act-mark act-mark-${mark.kind}`}
      style={{ left: `${pct(props.data, mark.ts)}%` }}
      title={title}
      aria-label={title}
      disabled={!key}
      onClick={() => key && props.onOpenIssue(key)}
    >
      <EventMark kind={mark.kind} title={title} />
    </button>
  );
}

/** Times along the top, the window before recording started, and "now" at the right edge. */
function Axis(props: { data: ActivityData; hours: Hours }) {
  const { data } = props;
  // Labels too close to the right edge would run into "now".
  const ticks = ticksOf(data, props.hours).filter((t) => pct(data, t) < 92);
  const recorded = data.recorded_since;
  return (
    <div className="act-row act-axis" aria-hidden="true">
      <div className="act-who" />
      <div className="act-track">
        {ticks.map((t) => (
          <span key={t} className="act-tick" style={{ left: `${pct(data, t)}%` }}>
            {clock(t)}
          </span>
        ))}
        {recorded !== null && recorded > data.from && (
          <span className="act-unrecorded-label" title={`Session states are recorded from ${clock(recorded)}. Before that Nitely only knows a session was active.`}>
            recorded from {clock(recorded)}
          </span>
        )}
        <span className="act-now-label">now</span>
      </div>
    </div>
  );
}

/** How many agents were working (and waiting on you) across the window. */
function Band(props: { data: ActivityData; now: number }) {
  const { data } = props;
  const { working, waiting } = useMemo(() => bucketCounts(data), [data]);
  const peak = Math.max(1, ...working.map((w, i) => w + waiting[i]));
  const h = 30;
  const x = (i: number) => (i / BUCKETS) * 1000;
  const y = (v: number) => h - (v / peak) * (h - 3);
  const step = (values: number[], base: number[]) => {
    let d = `M0 ${y(base[0])}`;
    values.forEach((v, i) => (d += ` L${x(i)} ${y(v + base[i])} L${x(i + 1)} ${y(v + base[i])}`));
    for (let i = values.length - 1; i >= 0; i--) d += ` L${x(i + 1)} ${y(base[i])} L${x(i)} ${y(base[i])}`;
    return `${d} Z`;
  };
  const zeros = working.map(() => 0);
  const nowWorking = data.lanes.filter((l) => liveness(l, props.now) === "working").length;
  return (
    <div className="act-row act-band-row">
      <div className="act-who act-band-label">
        <span>Working</span>
        <span className="muted small">
          {nowWorking} now · peak {Math.max(0, ...working)}
        </span>
      </div>
      <div className="act-track act-band">
        <Gridlines data={data} />
        <svg viewBox={`0 0 1000 ${h}`} preserveAspectRatio="none" role="img" aria-label={`Agents working over the last ${Math.round((data.to - data.from) / 3600)} hours, at most ${Math.max(0, ...working)} at once`}>
          <path d={step(working, zeros)} className="act-band-working" />
          <path d={step(waiting, working)} className="act-band-waiting" />
        </svg>
      </div>
    </div>
  );
}

function Gridlines(props: { data: ActivityData }) {
  const ticks = ticksOf(props.data, null);
  return (
    <>
      <span className="act-unrecorded" />
      {ticks.map((t) => (
        <span key={t} className="act-grid" style={{ left: `${pct(props.data, t)}%` }} />
      ))}
    </>
  );
}

function Legend(props: { data: ActivityData }) {
  const states: SegmentState[] = ["working", "waiting", "idle", "stale"];
  if (props.data.recorded_since !== null && props.data.recorded_since > props.data.from) states.push("seen");
  return (
    <div className="act-legend">
      <div className="act-legend-part">
        {states.map((s) => (
          <span key={s} className="act-legend-item">
            <span className={`act-swatch act-seg-${s}`} />
            {SEGMENT_LABEL[s]}
          </span>
        ))}
      </div>
      <div className="act-legend-part">
        {LEGEND.map((k) => (
          <span key={k} className="act-legend-item">
            <EventMark kind={k} />
            {EVENT_LABEL[k]}
          </span>
        ))}
      </div>
    </div>
  );
}

// ----- helpers -----

/** `?hours=24` in the URL opens that window, so a screenshot tool can pick it. */
function initialHours(): Hours {
  const h = Number(new URLSearchParams(location.search).get("hours"));
  return (WINDOWS as readonly number[]).includes(h) ? (h as Hours) : DEFAULT_HOURS;
}

function liveness(lane: ActivityLane, now: number): Liveness {
  return livenessFrom(lane.state, lane.last_seen_at, now);
}

function groupLanes(lanes: ActivityLane[], projects: Project[]) {
  const groups: { key: string; name: string; lanes: ActivityLane[] }[] = [];
  for (const lane of lanes) {
    const key = lane.project ?? "";
    let g = groups.find((x) => x.key === key);
    if (!g) {
      g = { key, name: key ? projects.find((p) => p.key === key)?.name ?? key : "Other folders", lanes: [] };
      groups.push(g);
    }
    g.lanes.push(lane);
  }
  // Projects with someone waiting on you first, then with work going on, then the rest.
  const rank = (g: (typeof groups)[number]) => (g.key === "" ? 9 : g.lanes.some((l) => l.state === "waiting") ? 0 : g.lanes.some((l) => l.state === "working") ? 1 : 2);
  return groups.sort((a, b) => rank(a) - rank(b));
}

function pct(data: ActivityData, ts: number) {
  return Math.min(100, Math.max(0, ((ts - data.from) / (data.to - data.from)) * 100));
}

function span(data: ActivityData, from: number, to: number) {
  const left = pct(data, from);
  return { left: `${left}%`, width: `${Math.max(0.15, pct(data, to) - left)}%` };
}

function unrecordedPct(data: ActivityData) {
  const r = data.recorded_since;
  return `${r === null ? 100 : pct(data, r)}%`;
}

/** Tick times at round local clock times. `hours` null: the window's own width picks the step. */
function ticksOf(data: ActivityData, hours: Hours | null): number[] {
  const width = (data.to - data.from) / 3600;
  const h = hours ?? WINDOWS.find((w) => w >= width - 0.01) ?? 24;
  const step = TICK_MIN[h] * 60;
  const offset = -new Date(data.from * 1000).getTimezoneOffset() * 60;
  const first = Math.ceil((data.from + offset) / step) * step - offset;
  const out: number[] = [];
  for (let t = first; t < data.to - step * 0.25; t += step) out.push(t);
  return out;
}

function bucketCounts(data: ActivityData) {
  const working = new Array<number>(BUCKETS).fill(0);
  const waiting = new Array<number>(BUCKETS).fill(0);
  const size = (data.to - data.from) / BUCKETS;
  for (const lane of data.lanes) {
    for (let i = 0; i < BUCKETS; i++) {
      const mid = data.from + (i + 0.5) * size;
      const seg: ActivitySegment | undefined = lane.segments.find((s) => s.from <= mid && mid < s.to);
      if (seg?.state === "working") working[i]++;
      else if (seg?.state === "waiting") waiting[i]++;
    }
  }
  return { working, waiting };
}

function clock(ts: number) {
  return new Date(ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function duration(secs: number) {
  const m = Math.round(secs / 60);
  if (m < 60) return `${m}m`;
  return `${Math.floor(m / 60)}h${m % 60 ? ` ${m % 60}m` : ""}`;
}
