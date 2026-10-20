import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import { VerificationAuditActions } from "./VerificationAuditActions";
import type { View } from "./App";
import { KIND_LABEL as DECISION_LABEL, type Decision, type DecisionKind } from "./decisions";
import type { Board, FlowIssue, GoalReport, Happening, IssueRecord, ProjectFlow, Pulse as PulseData, Session, TaskRow, VerificationAuditRate } from "./types";
import { HOME_LABEL, agentLabel, agentTitle, ago } from "./util";
import { GoalRisks, GoalRows } from "./GoalRows";
import { byProject, finishLine, groupKind, groupNeeded, neededLine, triagedLine } from "./goals";
import { LanesStrip } from "./LanesStrip";
import { useBatches } from "./BatchesStrip";
import { WaitingStrip, useWaits } from "./WaitingStrip";
import { landQueueLength } from "./batches";
import { auditedEscapeRate, verificationPresentation, verificationRepoName, verificationRevision } from "./verificationHealth";
import {
  AgentMark,
  EmptyState,
  IssueToken,
  Loading,
  STATION_LABEL,
  STATIONS,
  Sparkline,
  StationGlyph,
  StationTrack,
  Ticker,
  livenessLabel,
  livenessOf,
  phaseOf,
  trackOf,
  useFlip,
  type Station,
  type Track,
} from "./ui";
import "./styles/pulse.css";

const SEEN_KEY = "nitely.pulse.seen";
const DAY = 86400;
/** Issues shown per station before "+N more". */
const CHIPS = 3;

/** When the person last left Pulse. The first time there is no such visit: `first`, and the
 * last day is covered instead. */
function readSeen(now: number): { since: number; first: boolean } {
  try {
    const v = Number(localStorage.getItem(SEEN_KEY));
    if (v > 0 && v <= now) return { since: v, first: false };
  } catch {
    // Storage can be refused; then every visit covers the last day.
  }
  return { since: now - DAY, first: true };
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

/**
 * The track an issue's glyph draws: work that cannot move on (a rebase conflict, which its track
 * does not know) draws as stuck, wherever it is shown; work that is only quiet keeps its own.
 */
function glyphTrack(f: FlowIssue, row: TaskRow | undefined, now: number): Track {
  const t = trackOf(f.issue);
  const reason = stuckReason(f, row, now);
  const blocked = !!reason && !(f.quiet && !t.stuck && !row?.state?.rebase_in_progress);
  return blocked && !t.stuck ? { ...t, stuck: reason } : t;
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
export function Pulse(props: {
  board: Board | null;
  /** What waits on the human, the same list the Inbox shows and its badge counts. */
  decisions: Decision[] | null;
  now: number;
  version: number;
  onOpenIssue: (key: string) => void;
  onGo: (view: View) => void;
}) {
  const [{ since, first }] = useState(() => readSeen(Math.floor(Date.now() / 1000)));
  const [data, setData] = useState<PulseData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const batches = useBatches(props.version);
  const waits = useWaits(props.version);
  // When the data last seen on screen was read; "last seen" never moves past it. A refresh while
  // the window is hidden counts only once the window shows again.
  const shown = useRef<number | null>(null);
  const latest = useRef<number | null>(null);
  // Issues glide from one station box to the next as they move.
  const flows = useRef<HTMLDivElement>(null);
  useFlip(flows);

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
        <h1>{HOME_LABEL}</h1>
        <span className="head-note">{new Date(now * 1000).toLocaleString([], { weekday: "long", hour: "numeric", minute: "2-digit" })}</span>
        {data && <span className="head-note pulse-since">{sinceLine(data, first, ctx)}</span>}
      </div>
      {error && <p className="error">{error}</p>}
      {!data && !error && <Loading />}
      {data && (
        <>
          <Glance data={data} board={props.board} decisions={props.decisions} ctx={ctx} onGo={props.onGo} />
          {data.projects.length === 0 && (
            <EmptyState title="No projects yet." action={<button className="btn" onClick={() => props.onGo({ kind: "add" })}>Add a project</button>}>
              Add a repo and connect your agents; their work shows up here as it moves.
            </EmptyState>
          )}
          <RightNow data={data} board={props.board} ctx={ctx} />
          <LanesStrip version={props.version} now={now} onOpenIssue={props.onOpenIssue} landQueue={landQueueLength(batches)} />
          <WaitingStrip waits={waits} now={now} onOpenIssue={props.onOpenIssue} />
          <GoalRows heading version={props.version} onOpenIssue={props.onOpenIssue} />
          {data.projects.length > 0 && (
            <section>
              <h2>
                Flow <span className="head-note">open issues by station</span>
              </h2>
              <div className="flows" ref={flows}>
                {data.projects.map((p) => (
                  <Flow key={p.key} flow={p} ctx={ctx} onBoard={() => props.onGo({ kind: "project", key: p.key })} />
                ))}
              </div>
            </section>
          )}
          <Since data={data} ctx={ctx} first={first} />
        </>
      )}
    </div>
  );
}

/**
 * "12 got stuck, 1 still is": issues, not their failed runs (NIT-356). Which still are is the stuck
 * tile's own rule (`stuckReason`), so the line and the tile never disagree.
 */
function stuckLine(data: PulseData, ctx: Context): string | null {
  const stuck = data.stuck_issues;
  const failures = data.happened_counts.stuck;
  if (!stuck) return failures ? `${failures} ${failures === 1 ? "failure" : "failures"}` : null;
  if (!stuck.got) return null;
  const got = new Set(stuck.keys);
  const now = data.projects.flatMap((p) => p.issues).filter((f) => got.has(f.issue.key.toUpperCase()) && stuckReason(f, ctx.rows.get(f.issue.key), ctx.now)).length;
  const still = now === 0 ? "none still are" : `${now} still ${now === 1 ? "is" : "are"}`;
  return `${stuck.got} got stuck, ${still}`;
}

/** "Last here 10:36 · since then 1 landed, 1 question"; on a first visit, "In the last day: 1 landed". */
function sinceLine(data: PulseData, first: boolean, ctx: Context): string {
  const c = data.happened_counts;
  const parts = [
    c.landed && `${c.landed} landed`,
    c.asked && `${c.asked} ${c.asked === 1 ? "question" : "questions"}`,
    stuckLine(data, ctx),
  ].filter(Boolean);
  if (first) return parts.length ? `In the last day: ${parts.join(", ")}` : "Nothing in the last day";
  return `Last here ${when(data.since)} · ${parts.length ? `since then ${parts.join(", ")}` : "nothing new since"}`;
}

/** The four answers, before any detail. */
const DECISION_ORDER: DecisionKind[] = ["ask", "approve", "conflict", "input", "land"];

/** An open session and what it works on: one per session, however many share a folder (NIT-273). */
interface OpenSession {
  s: Session;
  key: string | null;
  issue: IssueRecord | null;
  report: string | null;
  repo: string | null;
}

/** Every session that is open, with the issue its own work names when there is one. */
function openSessions(board: Board | null, now: number): OpenSession[] {
  return [
    ...(board?.agents ?? []).map((a) => ({ s: a.session, key: a.issue_key, issue: a.issue, report: a.report, repo: a.repo })),
    // Agents on other machines reach the backlog through nitely serve, not a worktree here.
    ...(board?.remote_sessions ?? []).map((s) => ({ s, key: null, issue: null, report: null, repo: null })),
  ].filter(({ s }) => livenessOf(s, now) !== "ended");
}

function Glance({ data, board, decisions, ctx, onGo }: { data: PulseData; board: Board | null; decisions: Decision[] | null; ctx: Context; onGo: (view: View) => void }) {
  // "Need you" is the Inbox's list (decisionsOf), so this number and the Inbox badge never differ.
  // Until the board is in, the count is unknown, which is not the same as zero.
  const known = decisions !== null;
  const needs = decisions?.length ?? 0;
  const byKind = DECISION_ORDER.map((k) => ({ k, n: decisions?.filter((d) => d.kind === k).length ?? 0 })).filter((x) => x.n > 0);
  const oldest = decisions?.length ? Math.min(...decisions.map((d) => d.since)) : null;
  const sessions = openSessions(board, ctx.now);
  const working = sessions.filter(({ s }) => livenessOf(s, ctx.now) === "working");
  const waiting = sessions.filter(({ s }) => livenessOf(s, ctx.now) === "waiting");
  const flows = data.projects.flatMap((p) => p.issues);
  const stuck = flows.filter((f) => stuckReason(f, ctx.rows.get(f.issue.key), ctx.now));
  const done = data.projects.flatMap((p) => p.done_today);
  const landed = data.throughput.map((h) => h.landed);
  const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

  return (
    <div className="glance">
      <div className={`readout${known && needs ? " readout-attention" : " readout-calm"}`}>
        <span className="readout-top">
          <span className="readout-n" aria-label={known ? undefined : "not known yet"}>
            {known ? <Ticker value={needs} /> : "–"}
          </span>
          <span className="readout-label">{!known ? "need you" : needs ? (needs === 1 ? "needs you" : "need you") : "Nothing needs you"}</span>
        </span>
        <span className="readout-sub">
          {!known && "Reading the board"}
          {known && needs === 0 && "Agents can carry on alone"}
          {byKind.map(({ k, n }, i) => (
            <button key={k} className="link" onClick={() => onGo({ kind: "inbox" })}>
              {i > 0 && "· "}
              {plural(n, DECISION_LABEL[k], `${DECISION_LABEL[k]}s`)}
            </button>
          ))}
          {oldest !== null && <span>· oldest {ago(ctx.now, oldest).replace(" ago", "")}</span>}
        </span>
      </div>
      <div className={`readout${working.length ? " readout-live" : " readout-calm"}`}>
        <span className="readout-top">
          <span className="readout-n">
            <Ticker value={working.length} />
          </span>
          <span className="readout-label">{working.length === 1 ? "agent moving" : "agents moving"}</span>
        </span>
        <span className="readout-sub readout-marks">
          {working.map(({ s, key, report }) => (
            <AgentMark key={s.id} vendor={s.vendor} size={18} title={`${agentLabel(s.id)}${key ? ` on ${key}` : ""}: ${report ?? s.last_activity ?? "working"}`} />
          ))}
          {working.length === 0 && <span>{sessions.length ? "All quiet" : "No agent sessions open"}</span>}
          {waiting.length > 0 && <span className="readout-aside">{waiting.length} waiting on you</span>}
        </span>
      </div>
      <div className={`readout${stuck.length ? " readout-stuck" : " readout-calm"}`}>
        <span className="readout-top">
          <span className="readout-n">
            <Ticker value={stuck.length} />
          </span>
          <span className="readout-label">stuck</span>
        </span>
        <span className="readout-sub">
          {stuck.slice(0, 3).map((f) => (
            <IssueToken key={f.issue.key} issue={f.issue} track={glyphTrack(f, ctx.rows.get(f.issue.key), ctx.now)} onOpen={ctx.onOpenIssue} />
          ))}
          {stuck.length > 3 && <span>+{stuck.length - 3}</span>}
          {stuck.length === 0 && "Everything open is moving"}
        </span>
      </div>
      <div className={`readout${done.length ? "" : " readout-calm"}`}>
        <span className="readout-top">
          <span className="readout-n">
            <Ticker value={done.length} />
          </span>
          <span className="readout-label" title={`${data.landed_today} of them landed through Nitely; the rest were moved to Done`}>
            done today
          </span>
        </span>
        <span className="readout-sub">
          <Sparkline kind="bars" values={landed} width={130} height={18} color="var(--fg-3)" label={`Landed per hour, last 24 hours: ${landed.reduce((a, b) => a + b, 0)}`} />
        </span>
      </div>
    </div>
  );
}

const LIVE_ORDER = { working: 0, waiting: 1, idle: 2, stale: 3, ended: 4 } as const;

/** Every agent with a session open: who, on what, how far, and what it is doing now. */
function RightNow({ data, board, ctx }: { data: PulseData; board: Board | null; ctx: Context }) {
  const issues = new Map<string, IssueRecord>();
  for (const p of data.projects) for (const f of [...p.issues, ...p.done_today]) issues.set(f.issue.key, f.issue);
  const rank = (s: Session) => LIVE_ORDER[livenessOf(s, ctx.now) ?? "ended"];
  const sessions = openSessions(board, ctx.now).sort((a, b) => rank(a.s) - rank(b.s) || b.s.last_seen_at - a.s.last_seen_at);
  return (
    <section>
      <h2>
        Right now <span className="head-note">every agent with a session open</span>
      </h2>
      {sessions.length === 0 ? (
        <p className="muted small">No agent has a session open. Work they pick up shows here as it starts.</p>
      ) : (
        <div className="now-list">
          {sessions.map(({ s, key, issue: own, report, repo: where }) => {
            const issue = (key ? issues.get(key) : undefined) ?? own ?? undefined;
            const live = livenessOf(s, ctx.now);
            const waiting = live === "waiting";
            return (
              <button
                key={s.id}
                className={`now-row${waiting ? " is-waiting" : ""}${live === "idle" || live === "stale" ? " is-quiet" : ""}`}
                // A mirrored issue's token is a link to its tracker: that click is the link's alone.
                onClick={(e) => key && !(e.target as Element).closest("a") && ctx.onOpenIssue(key)}
                disabled={!key}
                title={[agentTitle(s.id), s.crew ? "started by the crew" : null, livenessLabel(s, ctx.now), s.last_activity].filter(Boolean).join(" · ")}
              >
                <span className="now-who">
                  <AgentMark vendor={s.vendor} state={live} />
                  <span className="now-who-name">
                    {agentLabel(s.id)}
                    {s.crew && <span className="muted"> · crew</span>}
                    {where && <span className="muted"> · {where.toLowerCase()}</span>}
                  </span>
                </span>
                <span className="now-issue">{issue ? <IssueToken issue={issue} title /> : <span className="muted">{key ?? "No issue"}</span>}</span>
                <span className="now-track">{issue && <StationTrack track={trackOf(issue)} />}</span>
                <span className={`now-step${waiting ? " attention-ink" : ""}`}>{waiting ? livenessLabel(s, ctx.now) : (report ?? s.last_activity ?? livenessLabel(s, ctx.now))}</span>
                <span className="now-age">{ago(ctx.now, waiting ? s.last_seen_at : s.started_at).replace(" ago", "")}</span>
              </button>
            );
          })}
        </div>
      )}
    </section>
  );
}

/** One project on the track: a box per station holding its issues as chips. */
function Flow({ flow, ctx, onBoard }: { flow: ProjectFlow; ctx: Context; onBoard: () => void }) {
  // Done today expands in place: the project page lists open issues by default.
  const [allDone, setAllDone] = useState(false);
  const at = (s: Station) =>
    s === "done" ? flow.done_today : flow.issues.filter((f) => trackOf(f.issue).station === s && !trackOf(f.issue).closed);
  const isStuck = (f: FlowIssue) => !!stuckReason(f, ctx.rows.get(f.issue.key), ctx.now);
  const stuck = flow.issues.filter(isStuck);
  const moving = flow.issues.filter((f) => phaseOf(trackOf(f.issue).station) === "agent" && !isStuck(f));
  const needs = at("needs").length;
  return (
    <div className="flow">
      <div className="flow-head">
        <button className="flow-name" onClick={onBoard} title={`Open ${flow.name}`}>
          {flow.name}
        </button>
        <span className="flow-key">{flow.key}</span>
        <span className="flow-sum">
          {flow.issues.length} open
          {moving.length > 0 && <> · {moving.length} moving</>}
          {needs > 0 && <span className="attention-ink"> · {needs} waiting on you</span>}
          {stuck.length > 0 && <span className="stuck-ink"> · {stuck.length} stuck</span>}
          {flow.done_today.length > 0 && <> · {flow.done_today.length} done today</>}
        </span>
      </div>
      <VerificationHealthRows flow={flow} ctx={ctx} />
      <VerificationAudits flow={flow} onOpenIssue={ctx.onOpenIssue} />
      <div className="flow-grid">
        {STATIONS.map((s) => {
          const here = at(s);
          const max = s === "done" && allDone ? here.length : CHIPS;
          return (
            <div key={s} className={`flow-box${here.length ? " has-items" : ""}${s === "needs" && here.length ? " is-needs" : ""}`} data-flip-group={`${flow.key}-${s}`}>
              <div className="flow-box-head">
                <span>{s === "done" ? "Done today" : STATION_LABEL[s]}</span>
                <Ticker className="flow-box-count" value={here.length} />
              </div>
              {here.slice(0, max).map((f) => (
                <Chip key={f.issue.key} f={f} ctx={ctx} />
              ))}
              {here.length > max && (
                <button className="flow-more" onClick={s === "done" ? () => setAllDone(true) : onBoard}>
                  +{here.length - max} more
                </button>
              )}
              {s === "done" && here.length === 0 && <span className="flow-more">none today</span>}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function VerificationHealthRows({ flow, ctx }: { flow: ProjectFlow; ctx: Context }) {
  if (!flow.verification_health?.length) return null;
  return <ul className="verification-health" aria-label={`${flow.name} verification health`}>
    {flow.verification_health.map((health) => {
      const presentation = verificationPresentation(health);
      const current = verificationRevision(health.observed_head);
      const verified = verificationRevision(health.verified_head);
      const recovery = health.recovery_issue && /^[a-z][a-z0-9]*-\d+$/i.test(health.recovery_issue) ? health.recovery_issue : null;
      const issue = recovery ? [...flow.issues, ...flow.done_today].find((f) => f.issue.key === recovery)?.issue : null;
      return <li className={`verification-health-row is-${presentation.tone}`} key={health.repo}>
        <div className="verification-health-main">
          <span className="verification-health-state"><span aria-hidden="true" className="verification-health-mark">{presentation.label === "Verified" ? "✓" : presentation.tone === "failed" ? "×" : presentation.tone === "paused" ? "Ⅱ" : "○"}</span>{presentation.label}</span>
          {flow.verification_health!.length > 1 && <span className="verification-health-repo">{verificationRepoName(health.repo)}</span>}
          <span className="verification-health-detail">{presentation.detail}</span>
          <span className="verification-health-age">{health.checked_at > 0 ? `Last Full pass ${ago(ctx.now, health.checked_at)}` : "No Full pass recorded"}</span>
        </div>
        <div className="verification-health-meta">
          {current && <span>Current <code>{current}</code></span>}
          {verified && current !== verified && <span>Last verified <code>{verified}</code></span>}
          {presentation.label === "Verification pending" && health.pending_since > 0 && <span>Pending for {ago(ctx.now, health.pending_since).replace(" ago", "")}</span>}
          {recovery && <span className="verification-health-recovery">Recovery {issue ? <IssueToken issue={issue} onOpen={ctx.onOpenIssue} /> : <a href={`#overview@${encodeURIComponent(recovery)}`} onClick={(e) => { e.preventDefault(); ctx.onOpenIssue(recovery); }}>{recovery}</a>}</span>}
        </div>
      </li>;
    })}
  </ul>;
}

function VerificationAudits({ flow, onOpenIssue }: { flow: ProjectFlow; onOpenIssue: (key: string) => void }) {
  const [report, setReport] = useState(flow.verification_audits);
  useEffect(() => setReport(flow.verification_audits), [flow.verification_audits]);
  if (!report || (!report.rules.length && !report.declarers.length && !report.jobs.length && !report.disabled_rules.length)) return null;
  const hasEscapes = [...report.rules, ...report.declarers].some((r) => r.confirmed_escape_lands > 0);
  const limited = report.rules_truncated || report.declarers_truncated || report.disabled_rules_truncated || report.escapes_truncated;
  const rates = (items: VerificationAuditRate[], declarers: boolean) => <ul className="verification-audit-rates">
    {items.map((rate) => <li key={rate.name}>
      <strong>{declarers ? agentLabel(rate.name) : rate.name.slice(0, 160)}</strong>
      <span>{rate.audited} / {rate.landed} audited · {Math.max(0, rate.landed - rate.audited)} unaudited, outcome unknown</span>
      <span className={rate.confirmed_escape_lands > 0 ? "stuck-ink" : ""}>{rate.confirmed_escape_lands} confirmed {rate.confirmed_escape_lands === 1 ? "escape" : "escapes"} · {auditedEscapeRate(rate)}</span>
      <span>{rate.declared_lands > 0 ? `${rate.matching_declarations} / ${rate.declared_lands} declarations agree with classified risk` : "No declarations to compare"}</span>
    </li>)}
  </ul>;
  return <details className="verification-audits">
    <summary><span>Verification audits</span><span className="verification-audit-summary">{report.rules.length}{report.rules_truncated ? "+" : ""} {report.rules.length === 1 ? "rule" : "rules"}{hasEscapes && <span className="stuck-ink"> · confirmed escapes</span>}{report.disabled_rules.length > 0 && ` · ${report.disabled_rules.length}${report.disabled_rules_truncated ? "+" : ""} paused`}</span></summary>
    <p className="verification-audit-note">Unaudited outcomes are unknown. Escape rates cover completed audits only; declaration agreement compares the declared risk with the classifier.</p>
    <VerificationAuditActions report={report} onChange={setReport} onOpenIssue={onOpenIssue} />
    {report.rules.length > 0 && <><h3>By rule</h3>{rates(report.rules, false)}</>}
    {report.declarers.length > 0 && <><h3>By declarer</h3>{rates(report.declarers, true)}</>}
    {report.disabled_rules.length > 0 && <p className="verification-audit-note">Paused rules: {report.disabled_rules.map((r) => r.slice(0, 160)).join(", ")}. These paths require Full verification.</p>}
    {limited && <p className="verification-audit-note">Showing a limited report; counts are per group and may overlap.</p>}
  </details>;
}

/** An issue at its station: its glyph and key, and who has it. Hover says the rest. */
function Chip({ f, ctx }: { f: FlowIssue; ctx: Context }) {
  const row = ctx.rows.get(f.issue.key);
  const t = trackOf(f.issue);
  const reason = stuckReason(f, row, ctx.now);
  const session = row?.session ?? f.session;
  const live = livenessOf(session, ctx.now);
  const vendor = session?.vendor ?? f.agent;
  const done = t.station === "done";
  const blocked = !!reason && !(f.quiet && !t.stuck && !row?.state?.rebase_in_progress);
  const hover = [
    `${f.issue.key}: ${f.issue.title}`,
    vendor && `${agentLabel(f.agent ?? vendor)}${live ? `, ${livenessLabel(session, ctx.now)}` : ""}`,
    t.rounds > 0 && `${t.rounds} round${t.rounds === 1 ? "" : "s"} of changes`,
    reason,
    f.last_activity && `last activity ${ago(ctx.now, f.last_activity)}`,
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <button
      className={`fchip${reason ? (blocked ? " is-stuck" : " is-quiet") : ""}${done ? " is-done" : ""}`}
      onClick={() => ctx.onOpenIssue(f.issue.key)}
      title={hover}
      data-flip={f.issue.key}
    >
      <StationGlyph track={glyphTrack(f, row, ctx.now)} size={12} />
      <span className="fchip-key">{f.issue.key}</span>
      {t.rounds > 0 && !done && <span className="fchip-rounds">↺{t.rounds}</span>}
      {vendor && !done && <AgentMark vendor={vendor} state={live === "waiting" ? "waiting" : null} size={14} />}
    </button>
  );
}

// "stuck" counts failed runs: gates, lands and reviews, each retry once (NIT-356).
const KIND_LABEL: Record<Happening["kind"], string> = {
  landed: "landed",
  filed: "filed",
  stuck: "failures",
  review: "review rounds",
  asked: "questions",
};
const KIND_ONE: Record<Happening["kind"], string> = { ...KIND_LABEL, stuck: "failure", review: "review round", asked: "question" };

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
      if (s.includes("Basic checks:")) return s.includes("waits for a human") ? "Basic checked, waits for you" : "Basic checked, review exempt";
      if (s.includes("YOLO mode")) return s.includes("waits for a human") ? "checks skipped, waits for you" : "checks skipped";
      if (s.startsWith("Submitted without a review")) return "submitted unreviewed, waits for you";
      return s.includes("waits for a human") ? "review passed, waits for you" : "review passed";
    case "stuck":
      return s.startsWith("Land stopped") ? "land stopped" : s.includes("did not finish") ? "review did not finish" : "gate failed";
  }
}

/** What happened while you were away, with landed and submitted per hour. */
function Since({ data, ctx, first }: { data: PulseData; ctx: Context; first: boolean }) {
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
        <h2>{first ? "The last day" : "Since you were last here"}</h2>
        <span className="muted small">
          {when(data.since)}, {ago(ctx.now, data.since)}
        </span>
      </div>
      {data.report && <ByGoal report={data.report} issues={issues} ctx={ctx} />}
      <div className="since-top">
        <div className="since-counts" role="group" aria-label="Filter what happened">
          {kinds.map((k) => (
            <button key={k} className={`since-count since-${k}${filter === k ? " on" : ""}`} onClick={() => setFilter(filter === k ? null : k)} aria-pressed={filter === k}>
              <span className="since-n">{count(k)}</span> {count(k) === 1 ? KIND_ONE[k] : KIND_LABEL[k]}
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
        <p className="muted small">
          {total === 0
            ? `Nothing ${filter ? `${KIND_LABEL[filter]} ` : ""}since then.`
            : `${total} ${filter ? KIND_LABEL[filter] : "events"}, all older than the newest ${data.happened.length} events. Each is on its issue's timeline.`}
        </p>
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
                <AgentMark vendor={h.actor} size={14} title={agentTitle(h.actor)} />
              </li>
            );
          })}
        </ol>
      )}
      {(all || shown.length <= 10) && shown.length > 0 && total > shown.length && (
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

/**
 * The same span by goal (NIT-247): per active goal what landed, what is at risk and when it will be
 * done at this pace, what landed outside any goal, and every place you were needed.
 */
function ByGoal({ report, issues, ctx }: { report: GoalReport; issues: Map<string, IssueRecord>; ctx: Context }) {
  const [allNeeded, setAllNeeded] = useState(false);
  const [allOther, setAllOther] = useState(false);
  const today = new Date(ctx.now * 1000);
  const needed = neededLine(report.needed);
  const triaged = triagedLine(report.triaged?.length ?? 0);
  const token = (key: string, title: string) => {
    const issue = issues.get(key);
    return issue ? (
      <IssueToken key={key} issue={issue} onOpen={ctx.onOpenIssue} />
    ) : (
      <button key={key} className="link mono small" onClick={() => ctx.onOpenIssue(key)} title={title}>
        {key}
      </button>
    );
  };
  if (report.goals.length === 0 && report.other_landed.length === 0 && !needed && !triaged) return null;
  // One row per issue, what still waits first (NIT-355).
  const groups = groupNeeded(report.needed);
  const shownNeeded = allNeeded ? groups : groups.slice(0, 5);
  return (
    <div className="by-goal">
      {report.goals.length > 0 && (
        <ul className="by-goal-list">
          {report.goals.map((g) => {
            const pace = finishLine(g.finish, g.goal.target_date, today);
            return (
              <li key={g.goal.id} className="by-goal-row">
                <div className="by-goal-head">
                  <span className="by-goal-title" title={g.goal.outcome}>
                    {g.goal.title}
                  </span>
                  <span className="by-goal-landed">
                    <span className="since-n">{g.landed.length}</span> landed
                  </span>
                  <span className="muted small">
                    {g.goal.progress.done}/{g.goal.progress.total - g.goal.progress.dropped} done
                  </span>
                  {pace && (
                    <span className={`small${pace.late ? " goal-late" : " muted"}`} title={pace.title}>
                      {pace.text}
                    </span>
                  )}
                </div>
                {g.landed.length > 0 && <div className="by-goal-keys">{g.landed.map((l) => token(l.key, l.title))}</div>}
                {g.risks.length > 0 && <GoalRisks risks={g.risks} onOpenIssue={ctx.onOpenIssue} />}
              </li>
            );
          })}
        </ul>
      )}
      {report.other_landed.length > 0 && (
        <p className="by-goal-other small">
          <span className="muted">Outside any goal, {report.other_landed.length} landed</span>
          <span>{byProject(report.other_landed).map((p) => `${p.project} ${p.count}`).join(" · ")}</span>
          <button className="link small" onClick={() => setAllOther(!allOther)} aria-expanded={allOther}>
            {allOther ? "Hide them" : "Show them"}
          </button>
          {allOther && <span className="by-goal-keys">{report.other_landed.map((l) => token(l.key, l.title))}</span>}
        </p>
      )}
      {triaged && (
        <p className="by-goal-triaged small" title={report.triaged.map((q) => `${q.issue_key ?? ""} ${q.question} → ${q.answer}`.trim()).join("\n")}>
          {triaged}
        </p>
      )}
      {needed && (
        <div className="by-goal-needed">
          <p className="small">{needed}</p>
          <ol className="by-goal-needs">
            {shownNeeded.map((n, i) => (
              <li key={`${n.issue_key ?? ""}-${n.ts}-${i}`} className={n.open ? "by-goal-open" : undefined}>
                <span className="feed-time">{clock(n.ts)}</span>
                <span className="by-goal-need-kind">{groupKind(n)}</span>
                {n.issue_key && (
                  <button className="link mono small" onClick={() => ctx.onOpenIssue(n.issue_key!)}>
                    {n.issue_key}
                  </button>
                )}
                <span className="by-goal-need-what" title={n.summary}>
                  {n.summary}
                </span>
                {n.open > 0 && <span className="by-goal-waits">{n.asked + n.approvals > 1 && n.open < n.asked + n.approvals ? `${n.open} wait` : "waits"}</span>}
              </li>
            ))}
          </ol>
          {groups.length > 5 && (
            <button className="link small since-all" onClick={() => setAllNeeded(!allNeeded)}>
              {allNeeded ? "Show the first 5" : `Show all ${groups.length} issues`}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
