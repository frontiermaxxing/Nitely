import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { Board, IssueRecord, PrRecord, SyncStatus, TaskRow } from "./types";

// Milestone 1 polls; a git and database file watcher replaces this later.
const REFRESH_MS = 5000;
// A session quiet for this long reads as stale even if it never sent an end event.
const STALE_SECS = 30 * 60;

export function App() {
  const [board, setBoard] = useState<Board | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showMain, setShowMain] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [now, setNow] = useState(() => Date.now() / 1000);

  const refresh = useCallback(async () => {
    try {
      setBoard(await invoke<Board>("snapshot"));
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    setNow(Date.now() / 1000);
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, REFRESH_MS);
    return () => clearInterval(id);
  }, [refresh]);

  const syncNow = async () => {
    setSyncing(true);
    try {
      await invoke("sync_now");
    } catch (e) {
      setError(String(e));
    }
    setSyncing(false);
    refresh();
  };

  const flaggedRows = board?.repos.flatMap((r) => r.rows).filter((r) => r.needs_me) ?? [];
  const needsMe = flaggedRows.length + (board?.needs_me_issues.length ?? 0);

  return (
    <div className="app">
      <header>
        <h1>Nitely</h1>
        <span className={needsMe ? "badge alert" : "badge"}>
          {needsMe ? `${needsMe} need${needsMe === 1 ? "s" : ""} you` : "Nothing needs you"}
        </span>
        <label className="toggle">
          <input type="checkbox" checked={showMain} onChange={(e) => setShowMain(e.target.checked)} />
          Show main checkouts
        </label>
      </header>

      {error && <p className="error">{error}</p>}
      {board?.repos.length === 0 && <Empty />}

      {board && board.needs_me_issues.length > 0 && (
        <section>
          <h2>Needs you, no worktree</h2>
          <ul className="plain">
            {board.needs_me_issues.map((i) => (
              <li key={i.key}>
                <IssueLink issue={i} /> {i.title}
              </li>
            ))}
          </ul>
        </section>
      )}

      {board?.repos.map((repo) => (
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
                  <Row key={row.worktree} row={row} now={now} />
                ))}
            </tbody>
          </table>
          {repo.unmatched_sessions.length > 0 && (
            <p className="muted small">
              {repo.unmatched_sessions.length} agent session(s) in this repo outside any worktree.
            </p>
          )}
        </section>
      ))}

      {board && (
        <footer>
          <SyncLine sync={board.sync} now={now} />
          <button onClick={syncNow} disabled={syncing}>
            {syncing ? "Syncing…" : "Sync now"}
          </button>
        </footer>
      )}
    </div>
  );
}

function Row({ row, now }: { row: TaskRow; now: number }) {
  const s = row.session;
  const stale = s && s.state !== "ended" && now - s.last_seen_at > STALE_SECS;
  const sessionLabel = !s ? "no session" : stale ? "stale" : s.state;
  const st = row.state;

  return (
    <tr className={row.needs_me ? "needs-me" : undefined} title={row.worktree}>
      <td>
        {row.issue ? (
          <>
            <IssueLink issue={row.issue} />
            <div className="small">{row.issue.title}</div>
            <span className={`chip state ${row.issue.state === "Needs Andrew" ? "waiting" : "none"}`}>
              {row.issue.state}
            </span>
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

function IssueLink({ issue }: { issue: IssueRecord }) {
  return (
    <a className="mono" href={issue.url} onClick={open(issue.url)}>
      {issue.key}
    </a>
  );
}

function PrCell({ pr }: { pr: PrRecord }) {
  const approvals = pr.approved_by.length;
  return (
    <>
      <a href={pr.url} onClick={open(pr.url)}>
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

/** Opens links in the system browser instead of inside the app window. */
function open(url: string) {
  return (e: React.MouseEvent) => {
    e.preventDefault();
    openUrl(url).catch(() => {});
  };
}

/** Needs-me first, then most recent agent activity, then most recent commit. */
function sortRows(rows: TaskRow[]): TaskRow[] {
  const key = (r: TaskRow) => Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0);
  return [...rows].sort((a, b) => Number(b.needs_me) - Number(a.needs_me) || key(b) - key(a));
}

function ago(now: number, ts: number): string {
  const s = Math.max(0, Math.round(now - ts));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}
