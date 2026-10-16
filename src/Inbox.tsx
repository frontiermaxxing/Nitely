import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { useEffect, useState } from "react";
import { invoke, isPreview } from "./backend";
import { decisionsOf, detailKeys, KIND_LABEL, type Decision, type DecisionKind } from "./decisions";
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
  StateDot,
  StationTrack,
  VENDOR_LABEL,
  journeyOf,
  livenessOf,
  trackOf,
  vendorOf,
} from "./ui";
import "./inbox.css";

/**
 * Every decision waiting on the human, one card each: what is asked, why it needs them, where the
 * issue is, and the action in place. Most urgent first, then longest waiting.
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

  return (
    <div className="page inbox">
      <div className="page-head">
        <h1>Inbox</h1>
        <Glance decisions={decisions} now={now} />
      </div>
      {decisions.length === 0 ? (
        <>
          <EmptyState title="Nothing needs you.">Agents show up here when they ask something, wait on your approval, or get stuck.</EmptyState>
          <AtWork board={board} now={now} onOpenIssue={props.onOpenIssue} />
        </>
      ) : (
        <div className="decisions">
          {decisions.map((d) => (
            <DecisionCard
              key={d.id}
              decision={d}
              detail={d.issue ? details.get(d.issue.key) : undefined}
              integration={board.repos.find((r) => r.name === d.repo)?.integration_branch ?? null}
              now={now}
              onOpenIssue={props.onOpenIssue}
              onChanged={props.onChanged}
            />
          ))}
        </div>
      )}
    </div>
  );
}

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

const PLURAL: Record<DecisionKind, string> = {
  ask: "questions",
  approve: "approvals",
  conflict: "conflicts",
  input: "waiting agents",
  land: "stopped lands",
};

/** The page in one line: how many decisions, of which kinds, and how long the oldest has waited. */
function Glance({ decisions, now }: { decisions: Decision[]; now: number }) {
  if (decisions.length === 0) return null;
  const kinds = (Object.keys(KIND_LABEL) as DecisionKind[])
    .map((k) => [k, decisions.filter((d) => d.kind === k).length] as const)
    .filter(([, n]) => n > 0);
  const oldest = Math.min(...decisions.map((d) => d.since));
  return (
    <span className="head-glance">
      <span className="attention-ink">
        <StateDot state="waiting" /> {decisions.length} wait{decisions.length === 1 ? "s" : ""} on you
      </span>
      <span>{kinds.map(([k, n]) => `${n} ${n === 1 ? KIND_LABEL[k] : PLURAL[k]}`).join(", ")}</span>
      <span>oldest {ago(now, oldest).replace(" ago", "")}</span>
    </span>
  );
}

function DecisionCard(props: {
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

  return (
    <article className={`decision decision-${d.kind}`}>
      <div className="decision-context">
        <span className="decision-kind">{KIND_WORD[d.kind]}</span>
        {issue ? (
          <IssueToken issue={issue} track={track ?? undefined} title onOpen={props.onOpenIssue} />
        ) : (
          <span className="mono muted">{d.row?.branch ?? d.repo ?? "No issue"}</span>
        )}
        <span className="decision-where">
          {track && <StationTrack track={track} />}
          <span className="decision-age" title={`Waiting since ${new Date(d.since * 1000).toLocaleString()}`}>
            {ago(now, d.since).replace(" ago", "")}
          </span>
        </span>
      </div>
      <Body decision={d} detail={detail} integration={props.integration} now={now} />
      <div className="actions">
        {d.kind === "ask" && d.ask && <Answer askId={d.ask.id} options={d.ask.options} busy={busy} act={act} />}
        {d.kind === "approve" && issue?.origin === "nitely" && (
          <>
            <button className="btn primary" disabled={busy} onClick={() => move("Approved")}>
              Approve
            </button>
            <button className="btn quiet" disabled={busy} onClick={() => move("Todo")}>
              Send back to Todo
            </button>
          </>
        )}
        {d.kind === "approve" && issue && issue.origin !== "nitely" && <span className="muted small">In {issue.origin}. Act on it there.</span>}
        {(d.kind === "conflict" || d.kind === "input" || d.kind === "land") && d.row && (
          <button className="btn primary" onClick={() => reveal(d.row!.worktree)}>
            Open the worktree
          </button>
        )}
        {d.kind === "land" && issue && (
          <button className="btn quiet" disabled={busy} onClick={() => move("Todo")}>
            Send back to Todo
          </button>
        )}
        {d.kind === "approve" && d.row && (
          <button className="btn quiet" onClick={() => reveal(d.row!.worktree)}>
            Open the worktree
          </button>
        )}
      </div>
      {(d.kind === "conflict" || d.kind === "input" || d.kind === "land") && d.row && <p className="decision-path mono">{d.row.worktree}</p>}
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
      const by = d.ask!.asked_by;
      return (
        <>
          <p className="decision-matter">{d.ask!.question}</p>
          <p className="decision-why">
            <AgentMark vendor={by} size={14} /> {who(by)} waits for your answer.
          </p>
        </>
      );
    }
    case "approve": {
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
                <AgentMark vendor={review.reviewer} size={14} /> {who(review.reviewer)}'s review passed
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
    <>
      {props.options.map((o) => (
        <button key={o} className="btn" disabled={props.busy} onClick={() => answer(o)}>
          {o}
        </button>
      ))}
      <form
        className="answer"
        onSubmit={(e) => {
          e.preventDefault();
          answer(text);
        }}
      >
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder={props.options.length ? "Or answer in your words" : "Your answer"} />
        <button className="btn primary" disabled={props.busy || !text.trim()}>
          Send
        </button>
      </form>
    </>
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
