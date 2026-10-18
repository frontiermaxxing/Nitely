import { useState } from "react";
import { invoke } from "./backend";
import type { View } from "./App";
import type { AgentRow, Board, SyncStatus, TaskRow } from "./types";
import { agentLabel, agentTitle, ago } from "./util";
import { exampleRepoPath } from "./AddProject";
import { LanesStrip } from "./LanesStrip";
import { BatchesStrip, useBatches } from "./BatchesStrip";
import { batchStation, landQueueLength } from "./batches";
import type { BatchesOverview } from "./types";
import {
  AgentMark,
  EmptyState,
  IssueToken,
  PrMark,
  STATION_LABEL,
  StateDot,
  StationTrack,
  Ticker,
  VENDOR_LABEL,
  livenessLabel,
  livenessOf,
  stationColor,
  trackOf,
  vendorOf,
} from "./ui";

/** Every worktree of every watched repo, with the agent in it and its git and PR state. */
export function Now(props: {
  board: Board;
  /** Moves on whenever the data changes, so what Now reads besides the board (lanes) reads again. */
  version: number;
  /** How many decisions wait on the human (the Inbox's count). */
  needs: number;
  now: number;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
  onGo: (view: View) => void;
}) {
  const { board, now } = props;
  const [showMain, setShowMain] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const batches = useBatches(props.version);

  const syncNow = async () => {
    setSyncing(true);
    await invoke("sync_now").catch(() => {});
    setSyncing(false);
    props.onChanged();
  };

  return (
    <div className="page">
      <div className="page-head">
        <h1>Now</h1>
        <Glance board={board} needs={props.needs} now={now} onInbox={() => props.onGo({ kind: "inbox" })} />
        <label className="toggle">
          <input type="checkbox" checked={showMain} onChange={(e) => setShowMain(e.target.checked)} />
          Show main checkouts
        </label>
      </div>
      <LanesStrip version={props.version} now={now} onOpenIssue={props.onOpenIssue} landQueue={landQueueLength(batches)} />
      <BatchesStrip batches={batches} now={now} onOpenIssue={props.onOpenIssue} />
      {board.repos.length === 0 && (
        <EmptyState title="No repos yet.">
          <p>Add one from the sidebar, or from a terminal:</p>
          <pre className="file">nitely connect {exampleRepoPath()} --yes</pre>
        </EmptyState>
      )}
      {board.repos.map((repo) => {
        const rows = sortRows(repo.rows, now).filter((r) => showMain || !r.is_main);
        return (
          <section key={repo.name}>
            <h2>
              {repo.name} <span className="head-note mono">{repo.integration_branch}</span>
            </h2>
            {repo.error && <p className="error">{repo.error}</p>}
            {rows.length === 0 ? (
              <p className="muted small">No task worktrees yet. Agents get one per issue; tick "Show main checkouts" to see the main one.</p>
            ) : (
              <div className="now-cards">
                {rows.map((row) => (
                  <Card key={row.worktree} row={row} integration={repo.integration_branch} batches={batches} now={now} onOpenIssue={props.onOpenIssue} />
                ))}
              </div>
            )}
            <Outside agents={outsideWorktrees(board, repo.name, now)} now={now} onOpenIssue={props.onOpenIssue} />
          </section>
        );
      })}
      {board.remote_sessions.length > 0 && <Remote sessions={board.remote_sessions} now={now} />}
      <footer>
        <SyncLine sync={board.sync} now={now} />
        <button className="btn" onClick={syncNow} disabled={syncing}>
          {syncing ? "Syncing…" : "Sync now"}
        </button>
      </footer>
    </div>
  );
}

/** A worktree as a card: who is in it, on what, how far it got, and its last three things. */
function Card(props: { row: TaskRow; integration: string; batches: BatchesOverview | null; now: number; onOpenIssue: (key: string) => void }) {
  const { row, now } = props;
  const s = row.session;
  const live = livenessOf(s, now);
  const st = row.state;
  const quiet = !live || live === "stale" || live === "ended" || live === "idle";
  const vendor = s?.vendor ?? row.vendor;
  const track = row.issue ? trackOf(row.issue) : null;
  const log: { t: string; text: string; tone?: "fail" | "attention" }[] = [];
  if (st?.rebase_in_progress) log.push({ t: "now", text: `Rebase onto ${props.integration} stopped on a conflict`, tone: "fail" });
  if (s?.last_activity) log.push({ t: live === "working" ? "now" : ago(now, s.last_seen_at).replace(" ago", ""), text: s.last_activity, tone: live === "waiting" ? "attention" : undefined });
  if (st?.last_commit_subject && st.last_commit_at) log.push({ t: ago(now, st.last_commit_at).replace(" ago", ""), text: st.last_commit_subject });
  if (s) log.push({ t: ago(now, s.started_at).replace(" ago", ""), text: `Session started${row.branch ? ` on ${row.branch}` : ""}` });
  // A batch holding the issue says so: "in a review batch with NIT-4", "landing, 2nd in the queue".
  const inBatch = row.issue ? batchStation(row.issue.key, props.batches) : null;
  const state = st?.rebase_in_progress
    ? { text: "Rebase stopped", color: "var(--fail)" }
    : track
      ? {
          text: track.stuck ?? (inBatch ? inBatch.charAt(0).toUpperCase() + inBatch.slice(1) : STATION_LABEL[track.station]),
          color: track.closed ? "var(--fg-3)" : stationColor(track.station, !!track.stuck),
        }
      : null;

  return (
    <div className={`now-card${row.needs_me || live === "waiting" ? " needs-me" : ""}${quiet && !row.needs_me ? " quiet" : ""}`} title={row.worktree}>
      <div className="now-card-top">
        {vendor ? <AgentMark vendor={vendor} state={live} size={20} /> : <span className="now-card-nobody" />}
        <span className="now-card-agent" title={s?.crew ? "Started by the project's crew" : undefined}>
          {vendor ? VENDOR_NAME[vendorOf(vendor)] : "No agent"}
          {s?.crew && <span className="muted"> · crew</span>}
        </span>
        <span className="now-card-branch">{row.branch ?? "detached"}</span>
        {row.pr && <PrMark pr={row.pr} link />}
        <span className="now-card-age">{s ? (live === "working" ? ago(now, s.started_at).replace(" ago", "") : livenessLabel(s, now)) : ""}</span>
      </div>
      {row.issue ? (
        <button
          className="now-card-issue"
          // A mirrored issue's token is a link to its tracker: that click is the link's alone.
          onClick={(e) => !(e.target as Element).closest("a") && props.onOpenIssue(row.issue!.key)}
        >
          <IssueToken issue={row.issue} title />
        </button>
      ) : (
        <span className="now-card-issue mono muted">{row.issue_key ?? (row.is_main ? "main checkout" : "No issue")}</span>
      )}
      <div className="now-card-state">
        {track && <StationTrack track={track} />}
        {state && (
          <span className="now-card-station" style={{ color: state.color }}>
            {state.text}
          </span>
        )}
        {st && (
          <span className="now-card-git" title={`ahead of and behind ${props.integration}`}>
            ↑{st.ahead} ↓{st.behind} · {st.dirty_files ? `${st.dirty_files} uncommitted` : "clean"}
          </span>
        )}
        {row.error && <span className="error small">{row.error}</span>}
      </div>
      {log.length > 0 && (
        <div className="now-card-log">
          {log.slice(0, 3).map((l, i) => (
            <div key={i} className={`now-log${l.tone ? ` now-log-${l.tone}` : ""}${i === 0 ? " first" : ""}`}>
              <span className="now-log-t">{l.t}</span>
              <span className="now-log-text">{l.text}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

const VENDOR_NAME: Record<string, string> = { ...VENDOR_LABEL, claude: "Claude Code" };

/** The repo's open sessions that run outside a task worktree (in its main checkout), newest first. */
function outsideWorktrees(board: Board, repo: string, now: number): AgentRow[] {
  return (board.agents ?? []).filter((a) => a.repo === repo && !a.worktree && livenessOf(a.session, now) !== "ended");
}

/**
 * Agents working from the repo's main checkout, one card each: a coordinator's lanes all run there
 * and work in worktrees by path, so each says the issue its own work names (NIT-273).
 */
function Outside({ agents, now, onOpenIssue }: { agents: AgentRow[]; now: number; onOpenIssue: (key: string) => void }) {
  if (agents.length === 0) return null;
  return (
    <>
      <h3 className="now-sub">
        In the main checkout <span className="head-note">{agents.length} session{agents.length === 1 ? "" : "s"}</span>
      </h3>
      <div className="now-cards">
        {agents.map((a) => {
          const s = a.session;
          const live = livenessOf(s, now);
          const quiet = !live || live === "stale" || live === "idle";
          const track = a.issue ? trackOf(a.issue) : null;
          return (
            <div key={s.id} className={`now-card${a.needs_me || live === "waiting" ? " needs-me" : ""}${quiet && !a.needs_me ? " quiet" : ""}`} title={agentTitle(s.id, s.cwd)}>
              <div className="now-card-top">
                <AgentMark vendor={s.vendor} state={live} size={20} />
                <span className="now-card-agent">
                  {agentLabel(s.id)}
                  {s.crew && <span className="muted"> · crew</span>}
                </span>
                <span className="now-card-age">{live === "working" ? ago(now, s.started_at).replace(" ago", "") : livenessLabel(s, now)}</span>
              </div>
              {a.issue ? (
                <button className="now-card-issue" onClick={(e) => !(e.target as Element).closest("a") && onOpenIssue(a.issue!.key)}>
                  <IssueToken issue={a.issue} title />
                </button>
              ) : (
                <span className="now-card-issue mono muted">{a.issue_key ?? "No issue"}</span>
              )}
              {track && (
                <div className="now-card-state">
                  <StationTrack track={track} />
                  <span className="now-card-station" style={{ color: track.closed ? "var(--fg-3)" : stationColor(track.station, !!track.stuck) }}>
                    {track.stuck ?? STATION_LABEL[track.station]}
                  </span>
                </div>
              )}
              <div className="now-card-log">
                <div className={`now-log first${live === "waiting" ? " now-log-attention" : ""}`}>
                  <span className="now-log-t">{live === "working" ? "now" : ago(now, s.last_seen_at).replace(" ago", "")}</span>
                  <span className="now-log-text">{a.report ?? s.last_activity ?? livenessLabel(s, now)}</span>
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </>
  );
}

/**
 * The agents moving now: in this machine's worktrees and on other machines (through `nitely
 * serve`). Now's header and the sidebar's Now count both say it, so the two always agree.
 */
export function movingCounts(board: Board, now: number): { here: number; elsewhere: number; total: number } {
  // Sessions, not folders: several agents often work from one main checkout (NIT-273).
  const here = (board.agents ?? []).filter((a) => livenessOf(a.session, now) === "working").length;
  const elsewhere = board.remote_sessions.filter((s) => livenessOf(s, now) === "working").length;
  return { here, elsewhere, total: here + elsewhere };
}

/** What a moving count is made of, for its hover. */
export const movingTitle = (m: { here: number; elsewhere: number }) =>
  m.elsewhere > 0 ? `${m.here} working here, ${m.elsewhere} on other machines` : `${m.here} working`;

/** The page in one line: how many agents move, wait on you, or sit idle. */
function Glance({ board, needs, now, onInbox }: { board: Board; needs: number; now: number; onInbox: () => void }) {
  const rows = board.repos.flatMap((r) => r.rows);
  const count = (l: string) => (board.agents ?? []).filter((a) => livenessOf(a.session, now) === l).length;
  const moving = movingCounts(board, now);
  const idle = count("idle") + count("stale");
  if (rows.length === 0 && moving.total === 0) return null;
  return (
    <>
      <span className="head-note live-ink" title={movingTitle(moving)}>
        <Ticker value={moving.total} /> moving
      </span>
      {needs > 0 && (
        <button className="link head-note attention-ink" onClick={onInbox}>
          <Ticker value={needs} /> waiting on you
        </button>
      )}
      {idle > 0 && (
        <span className="head-note">
          <Ticker value={idle} /> idle session{idle === 1 ? "" : "s"}
        </span>
      )}
    </>
  );
}

function SyncLine({ sync, now }: { sync: SyncStatus[]; now: number }) {
  if (sync.length === 0) {
    return <span className="muted small">Not synced with GitHub, Bitbucket or Linear yet.</span>;
  }
  return (
    <span className="small sync">
      {sync.map((s) => (
        <span key={s.source} className={s.last_error ? "error" : "muted"} title={s.last_error ?? undefined}>
          {s.source} {s.last_error ? "failed" : "synced"}
          {s.last_ok ? ` ${ago(now, s.last_ok)}` : ""}
        </span>
      ))}
    </span>
  );
}

const LIVE_ORDER: Record<string, number> = { working: 0, waiting: 1, idle: 2, stale: 3, ended: 4 };

/** Live work first, then what waits on you, then most recent agent activity or commit. */
function sortRows(rows: TaskRow[], now: number): TaskRow[] {
  const rank = (r: TaskRow) => LIVE_ORDER[livenessOf(r.session, now) ?? "ended"] ?? 5;
  const key = (r: TaskRow) => Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0);
  return [...rows].sort((a, b) => rank(a) - rank(b) || Number(b.needs_me) - Number(a.needs_me) || key(b) - key(a));
}

/** Agents on other machines on the local network: they reach the backlog, not a worktree here. */
function Remote({ sessions, now }: { sessions: Board["remote_sessions"]; now: number }) {
  const sorted = [...sessions].sort((a, b) => b.last_seen_at - a.last_seen_at);
  return (
    <section>
      <h2>
        On other machines <span className="muted">· through nitely serve</span>
      </h2>
      <div className="remote-list">
        {sorted.map((s) => {
          const live = livenessOf(s, now);
          return (
            <div className="remote-row" key={s.id}>
              <AgentMark vendor={s.id} state={live} />
              <span className="remote-who">
                {VENDOR_LABEL[vendorOf(s.id)]} <span className="muted">on {s.cwd.replace(/^remote:/, "")}</span>
              </span>
              <span className="remote-state small">
                <StateDot state={live} /> {livenessLabel(s, now)}
              </span>
              <span className="remote-doing small">{s.last_activity ?? <span className="muted">–</span>}</span>
              <span className="remote-end muted small">{ago(now, s.last_seen_at)}</span>
            </div>
          );
        })}
      </div>
    </section>
  );
}
