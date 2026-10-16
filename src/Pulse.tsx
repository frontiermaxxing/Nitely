import { useEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { invoke } from "./backend";
import type { View } from "./App";
import type { Board, FlowIssue, Happening, IssueRecord, ProjectFlow, Pulse as PulseData, Session, TaskRow } from "./types";
import { agentLabel, ago } from "./util";
import {
  AgentMark,
  CountBadge,
  EmptyState,
  IssueToken,
  STATION_LABEL,
  STATIONS,
  Sparkline,
  StateDot,
  livenessOf,
  stationColor,
  trackOf,
  type Station,
} from "./ui";
import "./styles/pulse.css";

const SEEN_KEY = "nitely.pulse.seen";
const DAY = 86400;
/** Cards shown per station before "+N more"; keys in the quiet Todo and Done columns. */
const CARDS = 4;
const KEYS = 6;

/** When the person last left Pulse; a day ago the first time. */
function readSeen(now: number): number {
  try {
    const v = Number(localStorage.getItem(SEEN_KEY));
    if (v > 0 && v <= now) return v;
  } catch {
    // Storage can be refused; then every visit covers the last day.
  }
  return now - DAY;
}

/** Saves the time of the data last shown, so nothing that was never on screen is skipped next time. */
function writeSeen(shown: number | null) {
  if (!shown) return;
  try {
    localStorage.setItem(SEEN_KEY, String(shown));
  } catch {
    // See readSeen.
  }
}

function localMidnight(): number {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return Math.floor(d.getTime() / 1000);
}

function clock(ts: number): string {
  return new Date(ts * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

/** "4:30 AM" today, "yesterday 4:30 AM", else "Tuesday 4:30 AM". */
function when(ts: number): string {
  const day = (t: number) => new Date(t * 1000).toDateString();
  const now = Date.now() / 1000;
  if (day(ts) === day(now)) return clock(ts);
  if (day(ts) === day(now - DAY)) return `yesterday ${clock(ts)}`;
  return `${new Date(ts * 1000).toLocaleDateString([], { weekday: "long" })} ${clock(ts)}`;
}

/** Why an issue cannot move on by itself, or null. Rebase conflicts come from the board's git state. */
function stuckReason(f: FlowIssue, row: TaskRow | undefined, now: number): string | null {
  if (row?.state?.rebase_in_progress) return "Rebase conflict";
  const t = trackOf(f.issue);
  if (t.stuck) return t.stuck;
  if (f.quiet) return `Quiet ${ago(now, f.last_activity).replace(" ago", "")}`;
  return null;
}

interface Context {
  now: number;
  rows: Map<string, TaskRow>;
  onOpenIssue: (key: string) => void;
}

/**
 * The home screen. The glance line says what needs you, what is moving, what is stuck and what
 * landed; each project's flow puts its open issues at their stations; and the bottom says what
 * happened since you last looked. One backend read (`pulse`), plus the board the app already has.
 */
export function Pulse(props: { board: Board | null; now: number; version: number; onOpenIssue: (key: string) => void; onGo: (view: View) => void }) {
  const [since] = useState(() => readSeen(Math.floor(Date.now() / 1000)));
  const [data, setData] = useState<PulseData | null>(null);
  const [error, setError] = useState<string | null>(null);
  // When the data last seen on screen was read; "last seen" never moves past it. A refresh while
  // the window is hidden counts only once the window shows again.
  const shown = useRef<number | null>(null);
  const latest = useRef<number | null>(null);

  useEffect(() => {
    let live = true;
    invoke<PulseData>("pulse", { since, dayStart: localMidnight() })
      .then((d) => {
        if (!live) return;
        setData(d);
        setError(null);
        latest.current = d.now;
        if (document.visibilityState === "visible") shown.current = d.now;
      })
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [since, props.version]);

  // "Since you were last here" moves on when you leave Pulse, not while you read it.
  useEffect(() => {
    const save = () => writeSeen(shown.current);
    const onHide = () => {
      if (document.visibilityState === "hidden") save();
      else shown.current = latest.current;
    };
    document.addEventListener("visibilitychange", onHide);
    window.addEventListener("pagehide", save);
    return () => {
      document.removeEventListener("visibilitychange", onHide);
      window.removeEventListener("pagehide", save);
      save();
    };
  }, []);

  const now = props.now;
  const rows = new Map<string, TaskRow>();
  for (const r of props.board?.repos.flatMap((repo) => repo.rows) ?? []) if (r.issue_key && !r.is_main) rows.set(r.issue_key, r);
  const ctx: Context = { now, rows, onOpenIssue: props.onOpenIssue };

  return (
    <div className="page pulse">
      <div className="page-head">
        <h1>Pulse</h1>
        <span className="head-note">{new Date(now * 1000).toLocaleString([], { weekday: "long", hour: "numeric", minute: "2-digit" })}</span>
      </div>
      {error && <p className="error">{error}</p>}
      {data && (
        <>
          <Glance data={data} board={props.board} ctx={ctx} onGo={props.onGo} />
          {data.projects.length === 0 && (
            <EmptyState title="No projects yet." action={<button className="btn" onClick={() => props.onGo({ kind: "add" })}>Add a project</button>}>
              Add a repo and connect your agents; their work shows up here as it moves.
            </EmptyState>
          )}
          {data.projects.map((p) => (
            <Flow key={p.key} flow={p} ctx={ctx} onBoard={() => props.onGo({ kind: "project", key: p.key })} />
          ))}
          <Since data={data} ctx={ctx} />
        </>
      )}
    </div>
  );
}

/** The four answers, before any detail. */
function Glance({ data, board, ctx, onGo }: { data: PulseData; board: Board | null; ctx: Context; onGo: (view: View) => void }) {
  const rows = board?.repos.flatMap((r) => r.rows) ?? [];
  const stuckTrees = rows.filter((r) => r.state?.rebase_in_progress || r.session?.state === "waiting");
  const needs = data.asks.length + data.approvals.length + stuckTrees.length;
  const sessions: { s: Session; key: string | null }[] = [
    ...rows.filter((r) => r.session).map((r) => ({ s: r.session!, key: r.issue_key })),
    ...(board?.repos.flatMap((r) => r.unmatched_sessions) ?? []).map((s) => ({ s, key: null })),
  ];
  const working = sessions.filter(({ s }) => livenessOf(s, ctx.now) === "working");
  const flows = data.projects.flatMap((p) => p.issues);
  const stuck = flows.filter((f) => stuckReason(f, ctx.rows.get(f.issue.key), ctx.now));
  const done = data.projects.flatMap((p) => p.done_today);
  const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

  return (
    <div className="glance">
      <div className={`readout${needs ? " readout-attention" : " readout-calm"}`}>
        <span className="readout-n">{needs}</span>
        <span className="readout-label">{needs ? (needs === 1 ? "needs you" : "need you") : "Nothing needs you"}</span>
        <span className="readout-sub">
          {data.asks.length > 0 && (
            <button className="link" onClick={() => onGo({ kind: "inbox" })}>
              {plural(data.asks.length, "question", "questions")}
            </button>
          )}
          {data.approvals.length > 0 && (
            <button className="link" onClick={() => onGo({ kind: "inbox" })}>
              {plural(data.approvals.length, "approval", "approvals")}
            </button>
          )}
          {stuckTrees.length > 0 && (
            <button className="link" onClick={() => onGo({ kind: "now" })}>
              {plural(stuckTrees.length, "worktree waiting", "worktrees waiting")}
            </button>
          )}
        </span>
      </div>
      <div className={`readout${working.length ? " readout-live" : " readout-calm"}`}>
        <span className="readout-n">{working.length}</span>
        <span className="readout-label">{working.length === 1 ? "agent working" : "agents working"}</span>
        <span className="readout-sub readout-marks">
          {working.map(({ s, key }) => (
            <AgentMark key={s.id} vendor={s.vendor} state="working" size={18} title={`${agentLabel(s.vendor)}${key ? ` on ${key}` : ""}: ${s.last_activity ?? "working"}`} />
          ))}
          {working.length === 0 && <span className="muted">{sessions.length ? "All quiet" : "No agent sessions today"}</span>}
        </span>
      </div>
      <div className={`readout${stuck.length ? " readout-stuck" : " readout-calm"}`}>
        <span className="readout-n">{stuck.length}</span>
        <span className="readout-label">stuck</span>
        <span className="readout-sub">
          {stuck.slice(0, 3).map((f) => (
            <IssueToken key={f.issue.key} issue={f.issue} onOpen={ctx.onOpenIssue} />
          ))}
          {stuck.length > 3 && <span className="muted">+{stuck.length - 3}</span>}
          {stuck.length === 0 && <span className="muted">Everything open is moving</span>}
        </span>
      </div>
      <div className={`readout${data.landed_today ? "" : " readout-calm"}`}>
        <span className="readout-n">{data.landed_today}</span>
        <span className="readout-label">landed today</span>
        <span className="readout-sub">
          {done.slice(0, 3).map((f) => (
            <IssueToken key={f.issue.key} issue={f.issue} onOpen={ctx.onOpenIssue} />
          ))}
          {done.length > 3 && <span className="muted">+{done.length - 3}</span>}
          {done.length === 0 && <span className="muted">Nothing yet today</span>}
        </span>
      </div>
    </div>
  );
}

/** One project on the track: a column per station, stuck work in its own band below. */
function Flow({ flow, ctx, onBoard }: { flow: ProjectFlow; ctx: Context; onBoard: () => void }) {
  // Done today expands in place: the project page lists open issues by default.
  const [allDone, setAllDone] = useState(false);
  const at = (s: Station) =>
    s === "done" ? flow.done_today : flow.issues.filter((f) => trackOf(f.issue).station === s && !trackOf(f.issue).closed);
  const isStuck = (f: FlowIssue) => !!stuckReason(f, ctx.rows.get(f.issue.key), ctx.now);
  const stuck = flow.issues.filter(isStuck);
  return (
    <section className="flow">
      <div className="flow-head">
        <h2>
          {flow.name} <span className="muted mono small">{flow.key}</span>
        </h2>
        <span className="flow-sum">
          {flow.issues.length} open
          {stuck.length > 0 && <span className="stuck-ink"> · {stuck.length} stuck</span>}
          {flow.done_today.length > 0 && <> · {flow.done_today.length} done today</>}
        </span>
        <button className="link flow-board" onClick={onBoard}>
          All issues
        </button>
      </div>
      {flow.issues.length === 0 && flow.done_today.length === 0 ? (
        <p className="muted small flow-empty">Nothing open.</p>
      ) : (
        <div className={`flow-grid${stuck.length ? " has-stuck" : ""}`}>
          {stuck.length > 0 && <div className="flow-band" aria-hidden="true" />}
          {STATIONS.map((s, i) => {
            const here = at(s);
            const moving = here.filter((f) => !isStuck(f));
            const blocked = here.filter(isStuck);
            // Placed explicitly, so the stuck band can lie under every column's last row.
            const style = { "--c": stationColor(s), "--col": i + 1 } as CSSProperties;
            return (
              <div key={s} className={`flow-col${here.length ? "" : " is-empty"}`} style={style}>
                <div className="flow-st">
                  <span className="flow-node" />
                  <span className="flow-st-label">{s === "done" ? "Done today" : STATION_LABEL[s]}</span>
                  <CountBadge count={here.length} tone={s === "needs" ? "attention" : "quiet"} />
                </div>
                {s === "todo" || s === "done" ? (
                  // Queued and finished work stays quiet: keys only, the title on hover.
                  <div className="flow-keys">
                    {moving.slice(0, s === "done" && allDone ? undefined : KEYS).map((f) => (
                      <IssueToken key={f.issue.key} issue={f.issue} onOpen={ctx.onOpenIssue} />
                    ))}
                    {moving.length > KEYS && !(s === "done" && allDone) && (
                      <button className="flow-more" onClick={s === "done" ? () => setAllDone(true) : onBoard}>
                        +{moving.length - KEYS} more
                      </button>
                    )}
                  </div>
                ) : (
                  <div className="flow-cards">
                    {moving.slice(0, CARDS).map((f) => (
                      <Card key={f.issue.key} f={f} ctx={ctx} />
                    ))}
                    {moving.length > CARDS && (
                      <button className="flow-more" onClick={onBoard}>
                        +{moving.length - CARDS} more
                      </button>
                    )}
                  </div>
                )}
                <div className="flow-cards flow-stuck">
                  {blocked.map((f) => (
                    <Card key={f.issue.key} f={f} ctx={ctx} />
                  ))}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

/** An issue at its station: who has it and whether they are live, its key, title, and why it is stuck. */
function Card({ f, ctx }: { f: FlowIssue; ctx: Context }) {
  const row = ctx.rows.get(f.issue.key);
  const t = trackOf(f.issue);
  const reason = stuckReason(f, row, ctx.now);
  const session = row?.session ?? f.session;
  const live = livenessOf(session, ctx.now);
  const waiting = live === "waiting";
  const vendor = session?.vendor ?? f.agent;
  const done = t.station === "done";
  const hover = [
    `${f.issue.key}: ${f.issue.title}`,
    vendor && `${agentLabel(f.agent ?? vendor)}${live ? `, ${live}` : ""}`,
    t.rounds > 0 && `${t.rounds} round${t.rounds === 1 ? "" : "s"} of changes`,
    reason,
    f.last_activity && `last activity ${ago(ctx.now, f.last_activity)}`,
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <button
      className={`fcard${reason ? (f.quiet && !t.stuck && !row?.state?.rebase_in_progress ? " is-quiet" : " is-stuck") : ""}${done ? " is-done" : ""}${waiting ? " is-waiting" : ""}`}
      onClick={() => ctx.onOpenIssue(f.issue.key)}
      title={hover}
    >
      <span className="fcard-top">
        {vendor ? <AgentMark vendor={vendor} state={done ? null : live} size={14} /> : <span className="fcard-nobody" />}
        <span className="fcard-key">{f.issue.key}</span>
        {t.rounds > 0 && !done && (
          <span className="fcard-rounds" aria-label={`${t.rounds} rounds of changes`}>
            ↺{t.rounds}
          </span>
        )}
        {(t.asking || waiting) && <StateDot state="waiting" title={waiting ? "Waiting for your input" : "Asked you something"} />}
      </span>
      <span className="fcard-title">{f.issue.title}</span>
      {reason && <span className="fcard-why">{reason}</span>}
    </button>
  );
}

const KIND_LABEL: Record<Happening["kind"], string> = {
  landed: "landed",
  filed: "filed",
  stuck: "stuck",
  review: "review rounds",
  asked: "questions",
};

function verb(h: Happening): string {
  const s = h.summary ?? "";
  switch (h.kind) {
    case "landed":
      return "landed";
    case "filed":
      return "filed";
    case "asked":
      return "asked you";
    case "review":
      if (s.includes("requested changes")) return "changes asked";
      if (s.startsWith("Submitted without a review")) return "submitted unreviewed, waits for you";
      return s.includes("waits for a human") ? "review passed, waits for you" : "review passed";
    case "stuck":
      return s.startsWith("Land stopped") ? "land stopped" : s.includes("did not finish") ? "review did not finish" : "gate failed";
  }
}

/** What happened while you were away, with landed and submitted per hour. */
function Since({ data, ctx }: { data: PulseData; ctx: Context }) {
  const [filter, setFilter] = useState<Happening["kind"] | null>(null);
  const [all, setAll] = useState(false);
  const issues = new Map<string, IssueRecord>();
  for (const p of data.projects) for (const f of [...p.issues, ...p.done_today]) issues.set(f.issue.key, f.issue);
  const count = (k: Happening["kind"]) => data.happened_counts[k];
  const shown = data.happened.filter((h) => !filter || h.kind === filter);
  // The list stops at the newest 300; the counts cover everything.
  const total = filter ? count(filter) : Object.values(data.happened_counts).reduce((a, b) => a + b, 0);
  const landed = data.throughput.map((h) => h.landed);
  const submitted = data.throughput.map((h) => h.submitted);
  const kinds: Happening["kind"][] = ["landed", "filed", "stuck", "review", "asked"];

  return (
    <section className="since">
      <div className="since-head">
        <h2>Since you were last here</h2>
        <span className="muted small">
          {when(data.since)}, {ago(ctx.now, data.since)}
        </span>
      </div>
      <div className="since-top">
        <div className="since-counts" role="group" aria-label="Filter what happened">
          {kinds.map((k) => (
            <button key={k} className={`since-count since-${k}${filter === k ? " on" : ""}`} onClick={() => setFilter(filter === k ? null : k)} aria-pressed={filter === k}>
              <span className="since-n">{count(k)}</span> {KIND_LABEL[k]}
            </button>
          ))}
        </div>
        <div className="since-charts">
          <span className="since-chart">
            <Sparkline kind="bars" values={landed} width={120} height={22} color="var(--live)" label={`Landed per hour, last 24 hours: ${landed.reduce((a, b) => a + b, 0)}`} />
            <span className="since-chart-label">landed / hour</span>
          </span>
          <span className="since-chart">
            <Sparkline kind="bars" values={submitted} width={120} height={22} label={`Submitted per hour, last 24 hours: ${submitted.reduce((a, b) => a + b, 0)}`} />
            <span className="since-chart-label">submitted / hour, 24h</span>
          </span>
        </div>
      </div>
      {shown.length === 0 ? (
        <p className="muted small">Nothing {filter ? `${KIND_LABEL[filter]} ` : ""}since then.</p>
      ) : (
        <ol className="feed">
          {(all ? shown : shown.slice(0, 10)).map((h, n) => {
            const issue = h.issue_key ? issues.get(h.issue_key) : undefined;
            return (
              <li key={`${h.ts}-${n}`} className={`feed-row feed-${h.kind}`}>
                <span className="feed-time">{clock(h.ts)}</span>
                <span className="feed-issue">
                  {issue ? (
                    <IssueToken issue={issue} onOpen={ctx.onOpenIssue} />
                  ) : (
                    h.issue_key && (
                      <button className="link mono small" onClick={() => ctx.onOpenIssue(h.issue_key!)}>
                        {h.issue_key}
                      </button>
                    )
                  )}
                </span>
                <span className="feed-verb">{verb(h)}</span>
                <span className="feed-what" title={h.summary ?? undefined}>
                  {issue?.title ?? h.summary}
                </span>
                <AgentMark vendor={h.actor} size={14} title={agentLabel(h.actor)} />
              </li>
            );
          })}
        </ol>
      )}
      {all && total > shown.length && (
        <p className="muted small since-cut">
          The newest {shown.length} of {total}. The rest are on each issue's timeline.
        </p>
      )}
      {shown.length > 10 && (
        <button className="link small since-all" onClick={() => setAll(!all)}>
          {all ? "Show fewer" : `Show all ${total > shown.length ? `latest ${shown.length}` : shown.length}`}
        </button>
      )}
    </section>
  );
}
