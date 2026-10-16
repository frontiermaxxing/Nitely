import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { useEffect, useState } from "react";
import { invoke, isPreview } from "./backend";
import { decisionsOf, detailKeys, type Decision, type DecisionKind } from "./decisions";
import { ChecksMark } from "./Pr";
import type { Board, IssueDetail, TaskRow } from "./types";
import { ago } from "./util";
import {
  AgentMark,
  CountBadge,
  EmptyState,
  FindingDots,
  IssueToken,
  describeFindings,
  LIVENESS_LABEL,
  StationTrack,
  VENDOR_LABEL,
  journeyOf,
  livenessOf,
  trackOf,
  vendorOf,
} from "./ui";
import "./inbox.css";

/**
 * Every decision waiting on the human. The list on the left groups them by kind, most urgent
 * first, then longest waiting; the decision picked is open on the right: what is asked, why it
 * needs you, where the issue is on the track, and the action in place.
 */
export function Inbox(props: {
  board: Board;
  /** From useDecisionDetails, which the app runs so the nav count and this page agree. */
  details: Map<string, IssueDetail>;
  now: number;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
}) {
  const { board, details, now } = props;
  const decisions = decisionsOf(board, details, now);
  const groups = GROUP_ORDER.map((k) => ({ kind: k, items: decisions.filter((d) => d.kind === k) })).filter((g) => g.items.length > 0);
  const ordered = groups.flatMap((g) => g.items);
  const [picked, setPicked] = useState<string | null>(null);
  // The first decision is open until you pick one; a picked one that was settled hands over to the first.
  const open = ordered.find((d) => d.id === picked) ?? ordered[0] ?? null;
  const oldest = decisions.length ? Math.min(...decisions.map((d) => d.since)) : null;

  if (decisions.length === 0) {
    return (
      <div className="page inbox">
        <div className="page-head">
          <h1>Inbox</h1>
        </div>
        <EmptyState title="Nothing needs you.">Agents show up here when they ask something, wait on your approval, or get stuck.</EmptyState>
        <AtWork board={board} now={now} onOpenIssue={props.onOpenIssue} />
      </div>
    );
  }

  return (
    <div className="inbox-split">
      <div className="inbox-list">
        <div className="inbox-head">
          <div className="page-head">
            <h1>Inbox</h1>
            <span className="head-note attention-ink">
              {decisions.length} need{decisions.length === 1 ? "s" : ""} you
            </span>
          </div>
          <p className="inbox-sub">Only decisions an agent cannot make.{oldest !== null && ` Oldest ${ago(now, oldest).replace(" ago", "")}.`}</p>
        </div>
        {groups.map((g) => (
          <div key={g.kind} className="inbox-group">
            <div className="inbox-group-head">
              <span>{GROUP_LABEL[g.kind]}</span>
              <span>{g.items.length}</span>
            </div>
            {g.items.map((d) => (
              <InboxRow
                key={d.id}
                decision={d}
                detail={d.issue ? details.get(d.issue.key) : undefined}
                on={open?.id === d.id}
                now={now}
                onPick={() => setPicked(d.id)}
                onOpenIssue={props.onOpenIssue}
              />
            ))}
          </div>
        ))}
        <div className="inbox-keys">
          <span>
            <kbd>j k</kbd> move
          </span>
          <span>
            <kbd>↵</kbd> open issue
          </span>
        </div>
      </div>
      {open && (
        <DecisionDetail
          key={open.id}
          decision={open}
          detail={open.issue ? details.get(open.issue.key) : undefined}
          integration={board.repos.find((r) => r.name === open.repo)?.integration_branch ?? null}
          now={now}
          onOpenIssue={props.onOpenIssue}
          onChanged={props.onChanged}
        />
      )}
    </div>
  );
}

/** Questions first: an agent is blocked on each one. */
const GROUP_ORDER: DecisionKind[] = ["ask", "approve", "conflict", "land", "input"];
const GROUP_LABEL: Record<DecisionKind, string> = {
  ask: "Questions",
  approve: "Approvals",
  conflict: "Conflicts",
  input: "Waiting agents",
  land: "Stopped lands",
};

/**
 * Details of the issues the Inbox's decisions name (why an approval waits, whether a land
 * stopped), refetched whenever the board changes.
 */
export function useDecisionDetails(board: Board | null): Map<string, IssueDetail> {
  const [details, setDetails] = useState<Map<string, IssueDetail>>(new Map());
  useEffect(() => {
    if (!board) return;
    let live = true;
    Promise.all(detailKeys(board).map((key) => invoke<IssueDetail>("issue_detail", { key }).catch(() => null))).then((all) => {
      if (!live) return;
      const next = new Map<string, IssueDetail>();
      for (const d of all) if (d) next.set(d.issue.key, d);
      setDetails(next);
    });
    return () => {
      live = false;
    };
  }, [board]);
  return details;
}

/** What last happened, in one line under a decision in the list: good is teal, bad is red. */
function eventOf(d: Decision, detail: IssueDetail | undefined): { text: string; tone: "good" | "bad" | null; agent: string | null } {
  const who = (vendor: string | null) => VENDOR_LABEL[vendorOf(vendor)];
  const agent = d.row?.session?.vendor ?? d.row?.vendor ?? null;
  switch (d.kind) {
    case "ask":
      return { text: `${who(d.ask!.asked_by)} waits for your answer`, tone: null, agent: d.ask!.asked_by };
    case "approve": {
      if (d.issue && d.issue.origin !== "nitely") return { text: `Waits in ${originName(d.issue.origin)}`, tone: null, agent };
      const review = detail?.reviews[0];
      if (!review?.reviewer) return { text: detail ? "No review ran" : "Reading its review", tone: detail ? "bad" : null, agent };
      const reviewed = journeyOf(detail?.timeline, detail?.reviews).rounds.filter((r) => r.review && r.review.verdict !== "none").length;
      const passed = review.verdict !== "changes";
      return {
        text: `${who(review.reviewer)} ${passed ? "passed" : "asked for changes"}${reviewed > 1 ? ` after ${reviewed} rounds` : ""}`,
        tone: passed ? "good" : "bad",
        agent: review.reviewer,
      };
    }
    case "conflict":
      return { text: "Rebase stopped on a conflict", tone: "bad", agent };
    case "input":
      return { text: `${who(agent)} waits in its terminal`, tone: null, agent };
    case "land":
      return { text: "Land stopped", tone: "bad", agent };
  }
}

function InboxRow(props: {
  decision: Decision;
  detail?: IssueDetail;
  on: boolean;
  now: number;
  onPick: () => void;
  onOpenIssue: (key: string) => void;
}) {
  const { decision: d } = props;
  const issue = props.detail?.issue ?? d.issue;
  const ev = eventOf(d, props.detail);
  return (
    <button
      className={`inbox-row${props.on ? " on" : ""}`}
      onClick={props.onPick}
      onFocus={props.onPick}
      onKeyDown={(e) => {
        if (e.key === "Enter" && issue) {
          e.preventDefault();
          props.onOpenIssue(issue.key);
        }
      }}
      aria-current={props.on ? "true" : undefined}
    >
      <span className={`inbox-kind inbox-kind-${d.kind}`} aria-label={KIND_WORD[d.kind]}>
        {KIND_CHAR[d.kind]}
      </span>
      <span className="inbox-row-text">
        <span className="inbox-row-top">
          <span className="inbox-row-key">{issue?.key ?? d.row?.branch ?? d.repo}</span>
          <span className="inbox-row-title">{issue?.title ?? (d.row ? d.row.worktree : "")}</span>
        </span>
        <span className="inbox-row-ev">
          {ev.agent && <AgentMark vendor={ev.agent} size={16} />}
          <span className={`inbox-ev-dot${ev.tone ? ` inbox-ev-${ev.tone}` : ""}`} />
          <span className={ev.tone === "bad" ? "inbox-ev-bad-text" : undefined}>{ev.text}</span>
        </span>
      </span>
      <span className="inbox-row-age" title={`Waiting since ${new Date(d.since * 1000).toLocaleString()}`}>
        {ago(props.now, d.since).replace(" ago", "")}
      </span>
    </button>
  );
}

/** The kind's mark in the list: a filled "?" asks something; a ringed mark waits on a decision. */
const KIND_CHAR: Record<DecisionKind, string> = { ask: "?", approve: "✓", conflict: "!", input: "…", land: "!" };

function DecisionDetail(props: {
  decision: Decision;
  detail?: IssueDetail;
  integration: string | null;
  now: number;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
}) {
  const { decision: d, detail, now } = props;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const issue = detail?.issue ?? d.issue;
  const track = issue ? trackOf(issue, detail?.timeline, detail?.reviews) : null;
  const agent = d.kind === "ask" ? d.ask!.asked_by : (d.row?.session?.vendor ?? d.row?.vendor ?? null);
  const live = livenessOf(d.row?.session, now);

  const act = async (run: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await run();
      setError(null);
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };
  const move = (state: string) => act(() => invoke("move_issue", { key: issue!.key, state, evidence: null, reason: null }));
  const reveal = async (path: string) => {
    if (isPreview) {
      setNote("In the app this opens the folder in Explorer.");
      return;
    }
    try {
      await revealItemInDir(path);
    } catch (e) {
      setError(String(e));
    }
  };
  const status = d.kind === "ask" ? "waiting for your answer" : live ? LIVENESS_LABEL[live] : "no session open";

  return (
    <article className={`inbox-detail decision-${d.kind}`}>
      <div className="dd-context">
        <span className="dd-kind">{KIND_WORD[d.kind]}</span>
        {issue && <span className="mono muted">{issue.key}</span>}
        <span className="dd-meta" title={`Waiting since ${new Date(d.since * 1000).toLocaleString()}`}>
          {[issue?.kind, `${d.kind === "ask" ? "asked" : "waiting"} ${ago(now, d.since)}`].filter(Boolean).join(" · ")}
        </span>
      </div>
      {issue ? (
        <button className="dd-title" onClick={() => props.onOpenIssue(issue.key)} title="Open the issue">
          {issue.title}
        </button>
      ) : (
        <p className="dd-title mono">{d.row?.branch ?? d.repo ?? "No issue"}</p>
      )}
      {track && (
        <div className="dd-track">
          <StationTrack track={track} size="full" now={now} />
        </div>
      )}
      <div className="dd-card">
        {agent ? <AgentMark vendor={agent} state={d.kind === "ask" ? null : live} size={22} /> : <span className="dd-nobody" />}
        <div className="dd-card-body">
          <p className="dd-who">{[agent && VENDOR_LABEL[vendorOf(agent)], d.row?.branch, status].filter(Boolean).join(" · ")}</p>
          <Body decision={d} detail={detail} integration={props.integration} now={now} />
          {(d.kind === "conflict" || d.kind === "input" || d.kind === "land") && d.row && <p className="decision-path mono">{d.row.worktree}</p>}
        </div>
      </div>
      <div className="dd-actions">
        {d.kind === "ask" && d.ask && <Answer askId={d.ask.id} options={d.ask.options} busy={busy} act={act} />}
        {d.kind === "approve" && issue?.origin === "nitely" && (
          <div className="actions">
            <button className="btn primary" disabled={busy} onClick={() => move("Approved")}>
              Approve
            </button>
            <button className="btn" disabled={busy} onClick={() => move("Todo")}>
              Send back to Todo
            </button>
          </div>
        )}
        {d.kind === "approve" && issue && issue.origin !== "nitely" && <span className="muted small">In {originName(issue.origin)}. Act on it there.</span>}
        {(d.kind === "conflict" || d.kind === "input" || d.kind === "land") && d.row && (
          <div className="actions">
            <button className="btn primary" onClick={() => reveal(d.row!.worktree)}>
              Open the worktree
            </button>
            {d.kind === "land" && issue && (
              <button className="btn" disabled={busy} onClick={() => move("Todo")}>
                Send back to Todo
              </button>
            )}
          </div>
        )}
      </div>
      <div className="dd-links">
        {issue && (
          <button className="link" onClick={() => props.onOpenIssue(issue.key)}>
            Open issue
          </button>
        )}
        {d.row && (d.kind === "approve" || d.kind === "ask") && (
          <button className="link" onClick={() => reveal(d.row!.worktree)}>
            Open the worktree
          </button>
        )}
      </div>
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </article>
  );
}

/** The kind of decision as the first word of its card, so a column of them scans at a glance. */
const KIND_WORD: Record<DecisionKind, string> = {
  ask: "Question",
  approve: "Approval",
  conflict: "Conflict",
  input: "Waiting",
  land: "Land stopped",
};

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** What is asked (the card's headline) and why it needs the human, by kind. */
function Body(props: { decision: Decision; detail?: IssueDetail; integration: string | null; now: number }) {
  const { decision: d, detail } = props;
  const row = d.row;
  const agent = row?.session?.vendor ?? row?.vendor ?? null;
  const live = livenessOf(row?.session, props.now);
  const who = (vendor: string | null) => VENDOR_LABEL[vendorOf(vendor)];

  switch (d.kind) {
    case "ask": {
      return <p className="decision-matter">{d.ask!.question}</p>;
    }
    case "approve": {
      // A mirrored issue waits for the human in its own tracker; Nitely ran no playbook on it.
      if (d.issue && d.issue.origin !== "nitely") {
        return <p className="decision-matter">It waits for you in {originName(d.issue.origin)} ({d.issue.state}).</p>;
      }
      const { rounds } = journeyOf(detail?.timeline, detail?.reviews);
      const review = detail?.reviews[0];
      const reasons = review?.reasons ?? [];
      const reviewed = rounds.filter((r) => r.review && r.review.verdict !== "none").length;
      const pr = detail?.prs[0];
      return (
        <>
          <p className="decision-matter">{reasons.length > 0 ? `${capital(listReasons(reasons))}.` : "The playbook wants a person to approve this change."}</p>
          <p className="decision-why">
            {review?.reviewer ? (
              <>
                <AgentMark vendor={review.reviewer} size={14} /> {who(review.reviewer)}'s review{" "}
                {review.verdict === "changes" ? <span className="decision-changes">asked for changes</span> : "passed"}
                {reviewed > 1 && ` after ${reviewed} rounds`}
                <span className="decision-findings">
                  <FindingDots findings={review.findings} />
                  {review.findings.length > 0 ? describeFindings(review.findings) : "no findings"}
                </span>
              </>
            ) : (
              detail && "No review ran."
            )}
            {pr?.checks && <ChecksMark checks={pr.checks} />}
          </p>
        </>
      );
    }
    case "conflict":
      return (
        <>
          <p className="decision-matter">The rebase{props.integration ? ` onto ${props.integration}` : ""} stopped on a conflict.</p>
          <p className="decision-why">
            {agent ? (
              <>
                <AgentMark vendor={agent} state={live} size={14} /> {who(agent)} is {live ? LIVENESS_LABEL[live] : "not running"} in the worktree
              </>
            ) : (
              "No agent is in the worktree"
            )}
            {row?.state && <span className="decision-branch">{branchLine(row.state, props.integration)}</span>}
          </p>
        </>
      );
    case "input": {
      const activity = (row?.session?.last_activity ?? "").replace(/^Waiting for input:\s*/i, "");
      return (
        <>
          <p className="decision-matter">{activity ? capital(activity) : "The agent waits for input."}</p>
          <p className="decision-why">
            <AgentMark vendor={agent} state="waiting" size={14} /> {who(agent)} asks in its own terminal; answer it there.
          </p>
        </>
      );
    }
    case "land": {
      const stopped = detail?.timeline.filter((e) => e.kind === "land_failed").pop();
      const what = (stopped?.summary ?? "").replace(/^Land stopped:\s*/, "");
      return (
        <>
          <p className="decision-matter">{what ? `${capital(what)}.` : "The land stopped."}</p>
          <p className="decision-why">
            {agent && <AgentMark vendor={agent} state={live} size={14} />} No agent is working on it now
            {row?.state && <span className="decision-branch">{branchLine(row.state, props.integration)}</span>}
          </p>
        </>
      );
    }
  }
}

function branchLine(st: NonNullable<TaskRow["state"]>, integration: string | null): string {
  const base = integration ? ` ${integration}` : "";
  return `· ${st.ahead} ahead, ${st.behind} behind${base}${st.dirty_files ? ` · ${st.dirty_files} uncommitted` : ""}`;
}

/** The approval reasons in one line; a playbook rule adds one per matching file, so long lists are cut. */
function listReasons(reasons: string[]): string {
  const shown = reasons.slice(0, 3).join("; ");
  return reasons.length > 3 ? `${shown}; and ${reasons.length - 3} more` : shown;
}

/** An ask's options as buttons, and a line for anything else. */
function Answer(props: { askId: number; options: string[]; busy: boolean; act: (run: () => Promise<unknown>) => Promise<void> }) {
  const [text, setText] = useState("");
  const answer = (value: string) => {
    if (value.trim()) props.act(() => invoke("answer_ask", { id: props.askId, answer: value.trim() }));
  };
  return (
    <div className="dd-answer">
      {props.options.map((o, i) => (
        <button key={o} className={`dd-option${i === 0 ? " primary" : ""}`} disabled={props.busy} onClick={() => answer(o)}>
          <span className="dd-option-n">{i + 1}</span>
          {o}
        </button>
      ))}
      <form
        className="dd-words"
        onSubmit={(e) => {
          e.preventDefault();
          answer(text);
        }}
      >
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder={props.options.length ? "Or answer in your words…" : "Your answer…"} />
        <button className="btn primary" disabled={props.busy || !text.trim()}>
          Send
        </button>
      </form>
    </div>
  );
}

/** When nothing needs the human: what the agents are doing meanwhile. */
function AtWork(props: { board: Board; now: number; onOpenIssue: (key: string) => void }) {
  const order = { working: 0, waiting: 1, idle: 2 } as Record<string, number>;
  const rows = props.board.repos
    .flatMap((r) => r.rows)
    .map((row) => ({ row, live: livenessOf(row.session, props.now) }))
    .filter((x) => x.live && x.live in order)
    .sort((a, b) => order[a.live!] - order[b.live!] || (b.row.session?.last_seen_at ?? 0) - (a.row.session?.last_seen_at ?? 0));
  const working = rows.filter((x) => x.live === "working").length;
  return (
    <section className="at-work">
      <h2>
        Meanwhile <CountBadge count={working} tone="live" />
      </h2>
      {rows.length === 0 ? (
        <p className="muted small">No agent is working right now.</p>
      ) : (
        <div className="list">
          {rows.map(({ row, live }) => (
            <div key={row.worktree} className={`at-work-row${live === "idle" ? " quiet" : ""}`}>
              <AgentMark vendor={row.session?.vendor ?? row.vendor} state={live} />
              {row.issue ? <IssueToken issue={row.issue} title onOpen={props.onOpenIssue} /> : <span className="mono muted">{row.branch}</span>}
              <span className="at-work-activity">{row.session?.last_activity}</span>
              <span className="decision-age">{row.session ? ago(props.now, row.session.last_seen_at).replace(" ago", "") : ""}</span>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

/** "linear" → "Linear": where a mirrored issue lives, as its tracker spells itself. */
function originName(origin: string): string {
  return origin === "linear" ? "Linear" : origin.charAt(0).toUpperCase() + origin.slice(1);
}
