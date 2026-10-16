import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Board, IssueRecord, PrRecord, SyncStatus, TaskRow } from "./types";
import { ago, openExternal } from "./util";

// A session quiet for this long reads as stale even if it never sent an end event.
const STALE_SECS = 30 * 60;

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
        <label className="toggle">
          <input type="checkbox" checked={showMain} onChange={(e) => setShowMain(e.target.checked)} />
          Show main checkouts
        </label>
      </div>
      {board.repos.length === 0 && <Empty />}
      {board.repos.map((repo) => (
        <section key={repo.name}>
          <h2>
            {repo.name} <span className="muted">· {repo.integration_branch}</span>
          </h2>
          {repo.error && <p className="error">{repo.error}</p>}
          <table>
            <thead>
              <tr>
                <th>Issue</th>
                <th>Agent</th>
                <th>Doing</th>
                <th>PR</th>
                <th>Branch</th>
                <th>vs {repo.integration_branch}</th>
                <th>Uncommitted</th>
                <th>Last commit</th>
              </tr>
            </thead>
            <tbody>
              {sortRows(repo.rows)
                .filter((r) => showMain || !r.is_main)
                .map((row) => (
                  <Row key={row.worktree} row={row} now={now} onOpenIssue={props.onOpenIssue} />
                ))}
              {!showMain && repo.rows.every((r) => r.is_main) && (
                <tr>
                  <td colSpan={8} className="muted">
                    No task worktrees yet. Agents get one per issue; tick "Show main checkouts" to see the main one.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
          {repo.unmatched_sessions.length > 0 && (
            <p className="muted small">{repo.unmatched_sessions.length} agent session(s) in this repo outside any worktree.</p>
          )}
        </section>
      ))}
      <footer>
        <SyncLine sync={board.sync} now={now} />
        <button className="btn" onClick={syncNow} disabled={syncing}>
          {syncing ? "Syncing…" : "Sync now"}
        </button>
      </footer>
    </div>
  );
}

function Row({ row, now, onOpenIssue }: { row: TaskRow; now: number; onOpenIssue: (key: string) => void }) {
  const s = row.session;
  const stale = s && s.state !== "ended" && now - s.last_seen_at > STALE_SECS;
  const sessionLabel = !s ? "no session" : stale ? "stale" : s.state;
  const st = row.state;

  return (
    <tr className={row.needs_me ? "needs-me" : undefined} title={row.worktree}>
      <td>
        {row.issue ? (
          <>
            <IssueLink issue={row.issue} onOpenIssue={onOpenIssue} />
            <div className="small">{row.issue.title}</div>
            <span className={`chip state ${row.issue.state.startsWith("Needs") ? "waiting" : "none"}`}>{row.issue.state}</span>
          </>
        ) : (
          <span className="mono">{row.issue_key ?? (row.is_main ? "main" : "–")}</span>
        )}
      </td>
      <td>
        {row.vendor ?? "–"} <span className={`chip ${stale ? "stale" : s?.state ?? "none"}`}>{sessionLabel}</span>
      </td>
      <td className="doing">
        {st?.rebase_in_progress && <strong>Rebase stopped on a conflict. </strong>}
        {s?.last_activity ?? <span className="muted">–</span>}
        {s && <span className="muted"> · {ago(now, s.last_seen_at)}</span>}
      </td>
      <td>{row.pr ? <PrCell pr={row.pr} /> : <span className="muted">–</span>}</td>
      <td className="mono">{row.branch ?? "detached"}</td>
      <td className="mono">{st ? `↑${st.ahead} ↓${st.behind}` : "–"}</td>
      <td>{st ? (st.dirty_files ? `${st.dirty_files} files` : "clean") : row.error ? "error" : "–"}</td>
      <td>
        {st?.last_commit_subject ?? "–"}
        {st?.last_commit_at && <span className="muted"> · {ago(now, st.last_commit_at)}</span>}
      </td>
    </tr>
  );
}

/** Native issues open in the panel; Linear ones open in the browser. */
function IssueLink({ issue, onOpenIssue }: { issue: IssueRecord; onOpenIssue: (key: string) => void }) {
  if (issue.origin === "nitely") {
    return (
      <button className="link mono" onClick={() => onOpenIssue(issue.key)}>
        {issue.key}
      </button>
    );
  }
  return (
    <a className="mono" href={issue.url} onClick={openExternal(issue.url)}>
      {issue.key}
    </a>
  );
}

function PrCell({ pr }: { pr: PrRecord }) {
  const approvals = pr.approved_by.length;
  return (
    <>
      <a href={pr.url} onClick={openExternal(pr.url)}>
        #{pr.number}
      </a>{" "}
      <span className={`chip pr-${pr.state.toLowerCase()}`}>{pr.state.toLowerCase()}</span>
      <div className="small muted">
        {pr.approved_by_me ? "you approved" : approvals ? `${approvals} approval${approvals === 1 ? "" : "s"}` : "no approvals"}
        {pr.comment_count > 0 && ` · ${pr.comment_count} comments`}
      </div>
    </>
  );
}

function SyncLine({ sync, now }: { sync: SyncStatus[]; now: number }) {
  if (sync.length === 0) {
    return <span className="muted small">Bitbucket and Linear not synced yet (reads ~/.aera-work/credentials.json).</span>;
  }
  return (
    <span className="small">
      {sync.map((s) => (
        <span key={s.source} className={s.last_error ? "error" : "muted"} title={s.last_error ?? undefined}>
          {s.source}: {s.last_error ? "failed" : "ok"}
          {s.last_ok ? ` (${ago(now, s.last_ok)})` : ""}
          {"  "}
        </span>
      ))}
    </span>
  );
}

function Empty() {
  return (
    <div className="empty">
      <p>No repos yet. Add one from a terminal:</p>
      <pre>nitely repo add C:\Development\AeraChromium --integration win-dev --name Aera</pre>
    </div>
  );
}

/** Needs-me first, then most recent agent activity, then most recent commit. */
function sortRows(rows: TaskRow[]): TaskRow[] {
  const key = (r: TaskRow) => Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0);
  return [...rows].sort((a, b) => Number(b.needs_me) - Number(a.needs_me) || key(b) - key(a));
}
