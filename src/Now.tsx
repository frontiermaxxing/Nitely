import { useState } from "react";
import { invoke } from "./backend";
import type { Board, SyncStatus, TaskRow } from "./types";
import { PrCell } from "./Pr";
import { ago } from "./util";
import {
  AgentMark,
  EmptyState,
  IssueToken,
  LIVENESS_LABEL,
  StateDot,
  StationTrack,
  VENDOR_LABEL,
  livenessOf,
  trackOf,
  vendorOf,
} from "./ui";

/** Every worktree of every watched repo, with the agent in it and its git and PR state. */
export function Now(props: { board: Board; now: number; onOpenIssue: (key: string) => void; onChanged: () => void }) {
  const { board, now } = props;
  const [showMain, setShowMain] = useState(false);
  const [syncing, setSyncing] = useState(false);

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
        <Glance board={board} now={now} />
        <label className="toggle">
          <input type="checkbox" checked={showMain} onChange={(e) => setShowMain(e.target.checked)} />
          Show main checkouts
        </label>
      </div>
      {board.repos.length === 0 && (
        <EmptyState title="No repos yet.">
          <p>Add one from the sidebar, or from a terminal:</p>
          <pre className="file">nitely repo add C:\Development\AeraChromium --integration win-dev --name Aera</pre>
        </EmptyState>
      )}
      {board.repos.map((repo) => {
        const rows = sortRows(repo.rows).filter((r) => showMain || !r.is_main);
        return (
          <section key={repo.name}>
            <h2>
              {repo.name} <span className="muted mono small">{repo.integration_branch}</span>
            </h2>
            {repo.error && <p className="error">{repo.error}</p>}
            {rows.length === 0 ? (
              <p className="muted small">No task worktrees yet. Agents get one per issue; tick "Show main checkouts" to see the main one.</p>
            ) : (
              <div className="table-wrap">
                <table className="now">
                  <thead>
                    <tr>
                      <th>Issue</th>
                      <th>Agent</th>
                      <th>Pull request</th>
                      <th>Branch</th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((row) => (
                      <Row key={row.worktree} row={row} integration={repo.integration_branch} now={now} onOpenIssue={props.onOpenIssue} />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            {repo.unmatched_sessions.length > 0 && (
              <p className="muted small">
                {repo.unmatched_sessions.length} agent session{repo.unmatched_sessions.length === 1 ? "" : "s"} in this repo outside any worktree.
              </p>
            )}
          </section>
        );
      })}
      <footer>
        <SyncLine sync={board.sync} now={now} />
        <button className="btn" onClick={syncNow} disabled={syncing}>
          {syncing ? "Syncing…" : "Sync now"}
        </button>
      </footer>
    </div>
  );
}

function Row(props: { row: TaskRow; integration: string; now: number; onOpenIssue: (key: string) => void }) {
  const { row, now } = props;
  const s = row.session;
  const live = livenessOf(s, now);
  const st = row.state;
  const quiet = !live || live === "stale" || live === "ended";

  return (
    <tr className={`${row.needs_me ? "needs-me" : ""}${quiet && !row.needs_me ? " quiet" : ""}`} title={row.worktree}>
      <td className="cell-issue">
        {row.issue ? (
          <>
            <div className="cell-line">
              <IssueToken issue={row.issue} onOpen={props.onOpenIssue} />
              <StationTrack track={trackOf(row.issue)} />
            </div>
            <div className="cell-sub">{row.issue.title}</div>
          </>
        ) : (
          <span className="mono">{row.issue_key ?? (row.is_main ? "main checkout" : "–")}</span>
        )}
      </td>
      <td className="cell-agent">
        {row.vendor || s ? (
          <>
            <div className="cell-line">
              <AgentMark vendor={s?.vendor ?? row.vendor} state={live} />
              <span>{VENDOR_LABEL[vendorOf(s?.vendor ?? row.vendor)]}</span>
              <span className={live === "waiting" ? "attention-ink" : "muted"}>{live ? LIVENESS_LABEL[live] : "no session"}</span>
              {s && <span className="muted">· {ago(now, s.last_seen_at)}</span>}
            </div>
            <div className="cell-sub">
              {st?.rebase_in_progress && <strong>Rebase stopped on a conflict. </strong>}
              {s?.last_activity ?? (st?.rebase_in_progress ? "" : <span className="muted">–</span>)}
            </div>
          </>
        ) : (
          <span className="muted">–</span>
        )}
      </td>
      <td className="cell-pr">{row.pr ? <PrCell pr={row.pr} /> : <span className="muted">–</span>}</td>
      <td className="cell-branch">
        <div className="mono branch" title={row.branch ?? undefined}>
          {row.branch ?? "detached"}
        </div>
        <div className="cell-sub">
          {st ? (
            <>
              <span className="mono" title={`ahead of and behind ${props.integration}`}>
                ↑{st.ahead} ↓{st.behind}
              </span>
              {" · "}
              {st.dirty_files ? `${st.dirty_files} uncommitted` : "clean"}
              {st.last_commit_at && <span className="muted"> · {ago(now, st.last_commit_at)}</span>}
            </>
          ) : row.error ? (
            <span className="error">{row.error}</span>
          ) : (
            "–"
          )}
        </div>
      </td>
    </tr>
  );
}

/** The page in one line: how many agents work, wait on you, or went quiet. */
function Glance({ board, now }: { board: Board; now: number }) {
  const rows = board.repos.flatMap((r) => r.rows);
  const count = (l: string) => rows.filter((r) => livenessOf(r.session, now) === l).length;
  const stuck = rows.filter((r) => r.needs_me).length;
  const working = count("working");
  const stale = count("stale");
  if (rows.length === 0) return null;
  return (
    <span className="head-glance">
      <span>
        <StateDot state="working" /> {working} working
      </span>
      {stuck > 0 && (
        <span className="attention-ink">
          <StateDot state="waiting" /> {stuck} need{stuck === 1 ? "s" : ""} you
        </span>
      )}
      {stale > 0 && (
        <span>
          <StateDot state="stale" /> {stale} stale
        </span>
      )}
    </span>
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

/** Needs-me first, then most recent agent activity, then most recent commit. */
function sortRows(rows: TaskRow[]): TaskRow[] {
  const key = (r: TaskRow) => Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0);
  return [...rows].sort((a, b) => Number(b.needs_me) - Number(a.needs_me) || key(b) - key(a));
}
