import type { Board, IssueRecord, PrRecord, RepoView, Session, TaskRow } from "./types";
import { reposOf } from "./repos";
import { ago } from "./util";
import {
  AgentMark,
  AheadBehind,
  EmptyState,
  IssueToken,
  LIVENESS_LABEL,
  PrMark,
  StateDot,
  VENDOR_LABEL,
  livenessOf,
  phaseOf,
  stationOfState,
  vendorOf,
  type Liveness,
} from "./ui";

/** A worktree quiet for this long is sprawl, unless it waits on the human. */
const IDLE_DAYS = 2;
const DAY = 86400;

export { reposOf } from "./repos";

/** One branch on the map: a task worktree, a worktree with no branch, or a branch with no worktree. */
interface Line {
  id: string;
  branch: string | null;
  worktree: string | null;
  ahead: number | null;
  behind: number | null;
  dirty: number;
  rebase: boolean;
  issueKey: string | null;
  issue: IssueRecord | null;
  vendor: string | null;
  session: Session | null;
  live: Liveness | null;
  pr: PrRecord | null;
  /** Last sign of life: the agent's last event or the last commit. */
  last: number | null;
  subject: string | null;
  needsMe: boolean;
  error: string | null;
  /** Why this line is sprawl, if it is. */
  loose: string | null;
}

function closedWord(issue: IssueRecord): string | null {
  if (issue.state_type === "completed") return "Landed; worktree left";
  if (issue.state_type === "canceled") return `${issue.state}; worktree left`;
  return null;
}

function fromRow(r: TaskRow, now: number): Line {
  const last = Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0) || null;
  const idle = last ? (now - last) / DAY : 0;
  const loose = r.needs_me
    ? null
    : !r.branch
      ? "Detached HEAD"
      : !r.issue_key
        ? "No issue"
        : !r.issue
          ? `${r.issue_key} is not in Nitely`
          : (closedWord(r.issue) ?? (idle >= IDLE_DAYS ? `Idle ${Math.floor(idle)} days` : null));
  return {
    id: r.worktree,
    branch: r.branch,
    worktree: r.worktree,
    ahead: r.state?.ahead ?? null,
    behind: r.state?.behind ?? null,
    dirty: r.state?.dirty_files ?? 0,
    rebase: !!r.state?.rebase_in_progress,
    issueKey: r.issue_key,
    issue: r.issue,
    vendor: r.session?.vendor ?? r.vendor,
    session: r.session,
    live: livenessOf(r.session, now),
    pr: r.pr,
    last,
    subject: r.state?.last_commit_subject ?? null,
    needsMe: r.needs_me,
    error: r.error,
    loose,
  };
}

function linesOf(repo: RepoView, now: number): { flight: Line[]; loose: Line[] } {
  // The main checkout is the spine, unless someone checked a task branch out there: then it is a line too.
  const worktrees = repo.rows
    .filter((r) => !r.is_main || (r.branch && r.branch !== repo.integration_branch))
    .map((r) => fromRow(r, now));
  const branches: Line[] = repo.branches.map((b) => {
    // The worktree may be gone while the issue still waits on the human (approval after a cleanup).
    const needsMe = !!b.issue && phaseOf(stationOfState(b.issue.state, b.issue.state_type)) === "human";
    return {
      id: `branch:${b.name}`,
      branch: b.name,
      worktree: null,
      ahead: b.ahead,
      behind: b.behind,
      dirty: 0,
      rebase: false,
      issueKey: b.issue_key,
      issue: b.issue,
      vendor: b.vendor,
      session: null,
      live: null,
      pr: b.pr,
      last: b.last_commit_at,
      subject: b.last_commit_subject,
      needsMe,
      error: null,
      loose: needsMe ? null : b.ahead === 0 ? "No worktree · merged" : "No worktree",
    };
  });
  const recent = (a: Line, b: Line) => (b.last ?? 0) - (a.last ?? 0);
  const flight = [...worktrees, ...branches]
    .filter((l) => !l.loose)
    .sort((a, b) => Number(b.needsMe) - Number(a.needsMe) || Number(b.live === "working") - Number(a.live === "working") || recent(a, b));
  const loose = [...worktrees, ...branches].filter((l) => l.loose).sort((a, b) => Number(!a.worktree) - Number(!b.worktree) || recent(a, b));
  return { flight, loose };
}

/**
 * The code picture behind a project, one map per repo: the integration branch as a spine with
 * every task branch forking off it, how far each has moved, the state of its worktree, and who
 * works in it. Loose ends (worktrees with no issue or idle for days, branches with no worktree)
 * gather at the bottom so they can be cleaned up.
 */
export function BranchMap(props: { board: Board; projectKey: string; now: number; onOpenIssue: (key: string) => void }) {
  const repos = reposOf(props.board, props.projectKey);
  if (repos.length === 0) {
    return (
      <EmptyState title="No repo is watched for this project.">
        Add its repo from the sidebar, or run <span className="mono">nitely repo add &lt;path&gt;</span>. Its branches and worktrees show up here.
      </EmptyState>
    );
  }
  return (
    <>
      {repos.map((repo) => (
        <RepoMap key={repo.root} repo={repo} now={props.now} onOpenIssue={props.onOpenIssue} titled={repos.length > 1} />
      ))}
    </>
  );
}

function RepoMap(props: { repo: RepoView; now: number; titled: boolean; onOpenIssue: (key: string) => void }) {
  const { repo, now } = props;
  const { flight, loose } = linesOf(repo, now);
  const all = [...flight, ...loose];
  // Bars scale to the branches in flight; a loose end hundreds of commits behind just fills its bar.
  const max = Math.max(1, ...(flight.length ? flight : all).map((l) => Math.max(l.ahead ?? 0, l.behind ?? 0)));
  const main = repo.rows.find((r) => r.is_main) ?? null;
  const last = all[all.length - 1];

  return (
    <section className="bmap-repo">
      <div className="bmap-head">
        {props.titled && <h2>{repo.name}</h2>}
        <RepoGlance flight={flight} loose={loose} />
      </div>
      {repo.error && <p className="error">{repo.error}</p>}
      <div className="bmap" role="table" aria-label={`Branches of ${repo.name}`}>
        <div className="bmap-cols" role="row">
          <span role="columnheader" className="bmap-c-graph" />
          <span role="columnheader">Branch</span>
          <span role="columnheader" className="bmap-c-ab">vs {repo.integration_branch}</span>
          <span role="columnheader">Issue</span>
          <span role="columnheader">Agent</span>
          <span role="columnheader">PR</span>
          <span role="columnheader" className="bmap-c-age">Active</span>
        </div>
        <Spine repo={repo} main={main} now={now} alone={all.length === 0} />
        {flight.map((l) => (
          <BranchLine key={l.id} line={l} base={repo.integration_branch} max={max} now={now} last={l === last} onOpenIssue={props.onOpenIssue} />
        ))}
        {loose.length > 0 && <LooseHead loose={loose} />}
        {loose.map((l) => (
          <BranchLine key={l.id} line={l} base={repo.integration_branch} max={max} now={now} last={l === last} onOpenIssue={props.onOpenIssue} />
        ))}
      </div>
      {all.length === 0 && <p className="muted small bmap-none">No task branches. Agents get a branch and a worktree per issue when they start one.</p>}
    </section>
  );
}

/** The repo in one line: agents working, worktrees waiting on you, stale sessions, and loose ends. */
function RepoGlance({ flight, loose }: { flight: Line[]; loose: Line[] }) {
  const worktrees = [...flight, ...loose].filter((l) => l.worktree).length;
  const working = flight.filter((l) => l.live === "working").length;
  const needs = [...flight, ...loose].filter((l) => l.needsMe).length;
  const stale = flight.filter((l) => l.live === "stale").length;
  return (
    <span className="head-glance">
      <span className="muted">
        {worktrees} worktree{worktrees === 1 ? "" : "s"}
      </span>
      {working > 0 && (
        <span>
          <StateDot state="working" /> {working} working
        </span>
      )}
      {needs > 0 && (
        <span className="attention-ink">
          <StateDot state="waiting" /> {needs} need{needs === 1 ? "s" : ""} you
        </span>
      )}
      {stale > 0 && (
        <span>
          <StateDot state="stale" /> {stale} stale
        </span>
      )}
      {loose.length > 0 && (
        <span className="muted">
          {loose.length} loose end{loose.length === 1 ? "" : "s"}
        </span>
      )}
    </span>
  );
}

/** The integration branch: the head of the spine, with the main checkout's own state. */
function Spine(props: { repo: RepoView; main: TaskRow | null; now: number; alone: boolean }) {
  const { repo, main, now } = props;
  const st = main?.state;
  const elsewhere = main?.branch && main.branch !== repo.integration_branch;
  // Read, but on no branch: its commit and edits are not the integration branch's.
  const detached = !!main && !main.branch && !!st;
  const own = !elsewhere && !detached;
  return (
    <div className={`bmap-line bmap-spine${props.alone ? " bmap-last" : ""}`} role="row" title={repo.root}>
      <span className="bmap-graph" aria-hidden="true">
        <span className="bmap-node bmap-node-spine" />
      </span>
      <span className="bmap-branch" role="cell">
        <span className="bmap-name bmap-name-spine">{repo.integration_branch}</span>
        <span className="bmap-sub">
          {elsewhere ? (
            <span className="attention-ink">Main checkout is on {main?.branch}</span>
          ) : detached ? (
            <span className="attention-ink" title={st.last_commit_subject ? `At “${st.last_commit_subject}”` : undefined}>
              Main checkout is detached
            </span>
          ) : st ? (
            <>
              main checkout {st.rebase_in_progress ? <strong className="attention-ink">rebase stopped</strong> : st.dirty_files ? `${st.dirty_files} uncommitted` : "clean"}
            </>
          ) : (
            <span className="muted">main checkout not read</span>
          )}
        </span>
      </span>
      <span className="bmap-ab" role="cell" />
      <span className="bmap-issue bmap-subject" role="cell">
        {own && st?.last_commit_subject && <span className="bmap-subject-text">{st.last_commit_subject}</span>}
      </span>
      <span className="bmap-agent" role="cell" />
      <span className="bmap-pr" role="cell" />
      <span className="bmap-age" role="cell" title={own && st?.last_commit_at ? `Last commit ${ago(now, st.last_commit_at)}` : undefined}>
        {own && st?.last_commit_at ? short(now - st.last_commit_at) : ""}
      </span>
    </div>
  );
}

function LooseHead({ loose }: { loose: Line[] }) {
  const count = (f: (l: Line) => boolean) => loose.filter(f).length;
  const noBranch = count((l) => !!l.worktree && !l.branch);
  const noIssue = count((l) => !!l.worktree && !!l.branch && (!l.issueKey || !l.issue));
  const left = count((l) => !!l.worktree && !!l.issue && l.issue.state_type !== "started" && l.issue.state_type !== "unstarted" && l.issue.state_type !== "backlog");
  const idle = count((l) => !!l.worktree && !!l.loose?.startsWith("Idle"));
  const noWorktree = count((l) => !l.worktree);
  const parts = [
    noIssue && `${noIssue} worktree${noIssue === 1 ? "" : "s"} with no issue`,
    noBranch && `${noBranch} with no branch`,
    left && `${left} left after the issue closed`,
    idle && `${idle} idle for days`,
    noWorktree && `${noWorktree} branch${noWorktree === 1 ? "" : "es"} with no worktree`,
  ].filter(Boolean);
  return (
    <div className="bmap-line bmap-group" role="row">
      <span className="bmap-graph" aria-hidden="true" />
      <span className="bmap-group-label" role="cell">
        <strong>Loose ends</strong> <span className="muted">{parts.join(" · ")}</span>
      </span>
    </div>
  );
}

/** "3h", "6d": time since the last sign of life. */
function short(secs: number): string {
  const s = Math.max(0, secs);
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m`;
  if (s < DAY) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / DAY)}d`;
}

function BranchLine(props: { line: Line; base: string; max: number; now: number; last: boolean; onOpenIssue: (key: string) => void }) {
  const { line: l, now } = props;
  const kind = !l.worktree ? "ref" : !l.branch ? "detached" : "worktree";
  // The node says who has the move: attention when it waits on you, live while an agent works.
  const node = l.needsMe ? "needs" : l.live === "working" ? "live" : kind === "worktree" && !l.loose ? "held" : "quiet";
  const faded = !!l.loose || l.live === "stale";
  const cls = ["bmap-line", `bmap-${kind}`, l.needsMe && "bmap-needs", faded && "bmap-faded", props.last && "bmap-last"].filter(Boolean).join(" ");
  const vendor = l.vendor ? vendorOf(l.vendor) : null;
  const showAgent = !!l.session || (vendor && vendor !== "other");
  const where = l.worktree ?? `${l.branch} (no worktree)`;

  return (
    <div className={cls} role="row" title={where}>
      <span className="bmap-graph" aria-hidden="true">
        <span className="bmap-fork" />
        <span className={`bmap-node bmap-node-${node}`} />
      </span>
      <span className="bmap-branch" role="cell">
        <span className="bmap-name">{l.branch ?? folderOf(l.worktree)}</span>
        <span className="bmap-sub">
          {l.loose && <span className="bmap-loose">{l.loose}</span>}
          {l.loose && kind !== "ref" && " · "}
          {kind !== "ref" && <WorktreeState line={l} />}
        </span>
      </span>
      <span className="bmap-ab" role="cell">
        <AheadBehind ahead={l.ahead} behind={l.behind} base={props.base} max={props.max} />
      </span>
      <span className="bmap-issue" role="cell">
        {l.issue ? (
          <IssueToken issue={l.issue} title onOpen={props.onOpenIssue} />
        ) : l.subject ? (
          <span className="bmap-subject-text" title={`Last commit: ${l.subject}`}>
            {l.subject}
          </span>
        ) : null}
      </span>
      <span className="bmap-agent" role="cell">
        {showAgent && (
          <>
            <AgentMark vendor={l.vendor} state={l.live} />
            <span className={l.live === "waiting" ? "attention-ink" : "muted"} title={l.session?.last_activity ?? undefined}>
              {l.live ? LIVENESS_LABEL[l.live] : VENDOR_LABEL[vendor ?? "other"]}
            </span>
          </>
        )}
      </span>
      <span className="bmap-pr" role="cell">
        {l.pr && <PrMark pr={l.pr} link />}
      </span>
      <span className="bmap-age" role="cell" title={l.last ? `Last ${l.session ? "activity" : "commit"} ${ago(now, l.last)}` : undefined}>
        {l.last ? short(now - l.last) : ""}
      </span>
    </div>
  );
}

/** The last part of a worktree path: what a detached worktree is known by. */
function folderOf(path: string | null): string {
  return path?.split(/[\\/]/).filter(Boolean).pop() ?? "";
}

function WorktreeState({ line: l }: { line: Line }) {
  if (l.error) return <span className="error">{l.error}</span>;
  if (l.rebase) {
    return (
      <>
        <strong className="attention-ink">Rebase stopped</strong>
        {l.dirty > 0 && ` · ${l.dirty} uncommitted`}
      </>
    );
  }
  return <>{l.dirty > 0 ? `${l.dirty} uncommitted` : "clean"}</>;
}
