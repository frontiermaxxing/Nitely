import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { typing } from "./Palette";
import { useEffect, useRef, useState } from "react";
import { invoke, isPreview } from "./backend";
import { PlaybookDiff } from "./PlaybookDiff";
import { decisionsOf, detailKeys, outcomeOf, shownDecisions, trackAfter, type Decision, type DecisionKind, type Move, type Resolved } from "./decisions";
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
  ResultMark,
  StationTrack,
  Ticker,
  VENDOR_LABEL,
  journeyOf,
  livenessOf,
  reducedMotion,
  trackOf,
  useFlip,
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
  const live = decisionsOf(board, details, now);
  // A decision the human acts on resolves at once and says what happened, in place, before the
  // next one takes its place; the board can take a moment longer to drop it, so those the Inbox
  // has moved on from stay hidden until it does.
  const [resolved, setResolved] = useState<Map<string, Resolved>>(new Map());
  const [gone, setGone] = useState<Set<string>>(new Set());
  const [failed, setFailed] = useState<Map<string, string>>(new Map());
  // Rows fading out after their confirmation, before the rows below glide up into the gap.
  const [leaving, setLeaving] = useState<Set<string>>(new Set());
  const list = useRef<HTMLDivElement>(null);
  useFlip(list);
  // What the open decision's keys do, set by its card each render.
  const keys = useRef<DecisionKeys>({});
  const decisions = shownDecisions(live, resolved, gone);
  const waiting = decisions.filter((d) => !resolved.has(d.id));
  const liveIds = live.map((d) => d.id).join(" ");
  useEffect(() => {
    const ids = new Set(liveIds.split(" "));
    setGone((g) => ([...g].every((id) => ids.has(id)) ? g : new Set([...g].filter((id) => ids.has(id)))));
  }, [liveIds]);
  const groups = GROUP_ORDER.map((k) => ({ kind: k, items: decisions.filter((d) => d.kind === k) })).filter((g) => g.items.length > 0);
  const ordered = groups.flatMap((g) => g.items);
  const [picked, setPicked] = useState<string | null>(null);
  // The decision open stays open while others arrive: the first one is picked as soon as there is
  // one, and only when the picked one is settled does the pick move to the (new) first.
  const open = ordered.find((d) => d.id === picked) ?? ordered[0] ?? null;
  useEffect(() => {
    if (open && open.id !== picked) setPicked(open.id);
  }, [open, picked]);
  const oldest = waiting.length ? Math.min(...waiting.map((d) => d.since)) : null;

  // The latest list, pick and resolutions, for the timer that moves on after a confirmation.
  const latest = useRef({ ordered, picked: open?.id ?? null, resolved, liveIds });
  latest.current = { ordered, picked: open?.id ?? null, resolved, liveIds };
  const timers = useRef(new Set<number>());
  useEffect(() => () => timers.current.forEach((t) => clearTimeout(t)), []);

  const moveOn = (id: string) => {
    const { ordered: shown, picked: on, resolved: held } = latest.current;
    const at = shown.findIndex((x) => x.id === id);
    const waits = (x: Decision) => x.id !== id && !held.has(x.id);
    const next = shown.slice(at + 1).find(waits) ?? shown.slice(0, Math.max(at, 0)).reverse().find(waits) ?? null;
    // Focus follows only when it was on this decision (its buttons are gone now) or nowhere.
    const focus = document.activeElement;
    const follow = !focus || focus === document.body || !!focus.closest(".inbox-detail, .inbox-row.on");
    // The next decision opens at once; the row done with fades, then leaves.
    if (on === id) {
      setPicked(next?.id ?? null);
      if (follow && next) requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-decision="${CSS.escape(next.id)}"]`)?.focus());
    }
    const remove = () => {
      // Hidden only while the board still names it: once dropped, it may come back as a new decision.
      if (latest.current.liveIds.split(" ").includes(id)) setGone((g) => new Set(g).add(id));
      setResolved((m) => without(m, id));
      setLeaving((s) => withoutId(s, id));
    };
    if (reducedMotion()) return remove();
    setLeaving((s) => new Set(s).add(id));
    const t = window.setTimeout(() => {
      timers.current.delete(t);
      remove();
    }, LEAVE_MS);
    timers.current.add(t);
  };

  const act = async (d: Decision, move: Move, run: () => Promise<unknown>, answer?: string) => {
    const started = Date.now();
    setFailed((m) => without(m, d.id));
    setResolved((m) => new Map(m).set(d.id, { decision: d, move, answer, done: false }));
    try {
      await run();
    } catch (e) {
      // The decision comes back as it was, with why.
      setResolved((m) => without(m, d.id));
      setFailed((m) => new Map(m).set(d.id, String(e)));
      return;
    }
    setResolved((m) => (m.has(d.id) ? new Map(m).set(d.id, { ...m.get(d.id)!, done: true }) : m));
    props.onChanged();
    const t = window.setTimeout(() => {
      timers.current.delete(t);
      moveOn(d.id);
    }, Math.max(HOLD_MS - (Date.now() - started), SETTLED_MS));
    timers.current.add(t);
  };

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

  // a, t, 1-9 and r act on the open decision, and only while the focus is on it: its row or its
  // card. Never on one in the background.
  const onKeyDown = (e: React.KeyboardEvent) => {
    // A held key repeats: it must not go on to approve or answer the decisions that follow.
    if (e.repeat || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
    const on = (e.target as HTMLElement).closest(".inbox-row.on, .inbox-detail");
    // Not while an issue sheet covers the Inbox: the decision under it is not the one in view.
    if (!on || document.querySelector(".panel")) return;
    const k = keys.current;
    const n = Number(e.key);
    const run =
      e.key === "a" ? k.approve : e.key === "t" ? k.todo : e.key === "r" ? k.reply : Number.isInteger(n) && n >= 1 && n <= 9 ? () => k.option?.(n - 1) : undefined;
    if (!run) return;
    e.preventDefault();
    run();
  };

  return (
    <div className="inbox-split" onKeyDown={onKeyDown}>
      <div className="inbox-list" ref={list}>
        <div className="inbox-head">
          <div className="page-head">
            <h1>Inbox</h1>
            {waiting.length > 0 && (
              <span className="head-note attention-ink">
                <Ticker value={waiting.length} /> need{waiting.length === 1 ? "s" : ""} you
              </span>
            )}
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
                resolved={resolved.get(d.id)}
                leaving={leaving.has(d.id)}
                on={open?.id === d.id}
                onStep={(by) => {
                  const at = ordered.findIndex((x) => x.id === d.id) + by;
                  const next = ordered[Math.max(0, Math.min(ordered.length - 1, at))];
                  setPicked(next.id);
                  document.querySelector<HTMLElement>(`[data-decision="${CSS.escape(next.id)}"]`)?.focus();
                }}
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
          <span>
            <kbd>a</kbd> approve
          </span>
          <span>
            <kbd>t</kbd> send back
          </span>
          <span>
            <kbd>1–9</kbd> answer
          </span>
          <span>
            <kbd>?</kbd> all keys
          </span>
        </div>
      </div>
      {open && (
        <DecisionDetail
          key={open.id}
          decision={open}
          detail={open.issue ? details.get(open.issue.key) : undefined}
          integration={board.repos.find((r) => r.name === open.repo)?.integration_branch ?? null}
          resolved={resolved.get(open.id)}
          error={failed.get(open.id) ?? null}
          now={now}
          onOpenIssue={props.onOpenIssue}
          onAct={(move, run, answer) => act(open, move, run, answer)}
          keys={keys}
        />
      )}
    </div>
  );
}

/** The open decision's keys; a key it does not offer is left out. */
interface DecisionKeys {
  approve?: () => void;
  todo?: () => void;
  option?: (i: number) => void;
  reply?: () => void;
}

/** How long a decision the human acted on stays, saying what happened, before the next one takes its place. */
const HOLD_MS = 1500;
/** And at least this long after the call came back, when it took longer than that. */
const SETTLED_MS = 600;
/** How long a row done with takes to fade before it leaves the list (--dur). */
const LEAVE_MS = 200;

function withoutId(s: Set<string>, id: string): Set<string> {
  if (!s.has(id)) return s;
  const n = new Set(s);
  n.delete(id);
  return n;
}

function without<V>(m: Map<string, V>, id: string): Map<string, V> {
  if (!m.has(id)) return m;
  const n = new Map(m);
  n.delete(id);
  return n;
}

/** Questions first: an agent is blocked on each one. The human's own work blocks no agent directly, so it comes last. */
const GROUP_ORDER: DecisionKind[] = ["ask", "approve", "conflict", "land", "input", "own"];
const GROUP_LABEL: Record<DecisionKind, string> = {
  ask: "Questions",
  approve: "Approvals",
  own: "Your own work",
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

/**
 * The playbook change an issue's decision is about: pending, or just applied or rejected (the card
 * held after acting). One replaced by another proposal is that one's decision.
 */
function proposalOf(detail: IssueDetail | undefined) {
  const p = detail?.proposal;
  return p && (p.status === "pending" || p.status === "applied" || p.status === "rejected") ? p : null;
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
      const proposal = proposalOf(detail);
      if (proposal) return { text: `${who(proposal.created_by)} proposes a playbook change`, tone: null, agent: proposal.created_by };
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
    case "own":
      return { text: "Only you can do this", tone: null, agent: null };
    case "conflict":
      return { text: "Rebase stopped on a conflict", tone: "bad", agent };
    case "input":
      return { text: `${who(agent)} waits in its terminal`, tone: null, agent };
    case "land":
      return { text: "Land stopped", tone: "bad", agent };
  }
}

/**
 * A decision in the list. Only the open one is a tab stop, so Tab goes from the list straight to
 * its actions; j and k (and the arrow keys) move the pick, and focus follows it.
 */
function InboxRow(props: {
  decision: Decision;
  detail?: IssueDetail;
  resolved?: Resolved;
  leaving: boolean;
  on: boolean;
  now: number;
  onPick: () => void;
  onStep: (by: 1 | -1) => void;
  onOpenIssue: (key: string) => void;
}) {
  const { decision: d } = props;
  const issue = props.detail?.issue ?? d.issue;
  const done = props.resolved;
  const ev = done ? { text: outcomeOf(done), tone: "good" as const, agent: null } : eventOf(d, props.detail);
  return (
    <button
      className={`inbox-row${props.on ? " on" : ""}${done ? " resolved" : ""}${props.leaving ? " leaving" : ""}`}
      data-decision={d.id}
      data-flip={d.id}
      tabIndex={props.on ? 0 : -1}
      onClick={props.onPick}
      onKeyDown={(e) => {
        if (e.key === "Enter" && issue) {
          e.preventDefault();
          props.onOpenIssue(issue.key);
        } else if ((e.key === "j" || e.key === "ArrowDown" || e.key === "k" || e.key === "ArrowUp") && !e.ctrlKey && !e.metaKey && !e.altKey) {
          // Handled here rather than by the page-wide j/k, which only moves focus.
          e.preventDefault();
          e.stopPropagation();
          props.onStep(e.key === "j" || e.key === "ArrowDown" ? 1 : -1);
        }
      }}
      aria-current={props.on ? "true" : undefined}
    >
      <span className={`inbox-kind inbox-kind-${d.kind}`} aria-label={done ? "Done" : KIND_WORD[d.kind]}>
        {done ? "✓" : KIND_CHAR[d.kind]}
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
const KIND_CHAR: Record<DecisionKind, string> = { ask: "?", approve: "✓", own: "•", conflict: "!", input: "…", land: "!" };

function DecisionDetail(props: {
  decision: Decision;
  detail?: IssueDetail;
  integration: string | null;
  /** Set once the human acted on it: the card says what happened instead of offering it again. */
  resolved?: Resolved;
  /** Why the last action on it failed. */
  error: string | null;
  now: number;
  onOpenIssue: (key: string) => void;
  onAct: (move: Move, run: () => Promise<unknown>, answer?: string) => Promise<void>;
  keys: React.MutableRefObject<DecisionKeys>;
}) {
  const { decision: d, now, resolved } = props;
  // A decision just acted on can leave the details the Inbox reads (an applied playbook change is
  // Done at once): its card keeps showing what it was about until the Inbox moves on.
  const lastDetail = useRef(props.detail);
  if (props.detail) lastDetail.current = props.detail;
  const kept = lastDetail.current?.issue.key === d.issue?.key ? lastDetail.current : undefined;
  const detail = props.detail ?? (resolved ? kept : undefined);
  const busy = !!resolved;
  const [revealError, setError] = useState<string | null>(null);
  const error = props.error ?? revealError;
  const [note, setNote] = useState<string | null>(null);
  const issue = detail?.issue ?? d.issue;
  const at = issue ? trackOf(issue, detail?.timeline, detail?.reviews) : null;
  // The track moves to where the move takes it at once, before the backend says so.
  const track = at && resolved ? trackAfter(at, resolved.move, now) : at;
  // A playbook change an agent proposed: approving applies it, canceling with a reason rejects it.
  const proposal = d.kind === "approve" ? proposalOf(detail) : null;
  const pending = proposal?.status === "pending";
  const agent = d.kind === "ask" ? d.ask!.asked_by : (d.row?.session?.vendor ?? d.row?.vendor ?? proposal?.created_by ?? null);
  const live = livenessOf(d.row?.session, now);

  const move = (state: "Approved" | "Todo") =>
    props.onAct(state === "Approved" ? "approve" : "todo", () => invoke("move_issue", { key: issue!.key, state, evidence: null, reason: null }));
  const native = issue?.origin === "nitely";
  const card = useRef<HTMLElement>(null);
  const answer = (value: string) => {
    if (!d.ask || !value.trim()) return;
    const id = d.ask.id;
    props.onAct("answer", () => invoke("answer_ask", { id, answer: value.trim() }).then(() => drafts.delete(id)), value.trim());
  };
  // The keys do what the card's buttons do, and nothing once it is resolved.
  props.keys.current = resolved
    ? {}
    : {
        approve: d.kind === "approve" && native ? () => move("Approved") : undefined,
        todo: (d.kind === "approve" || d.kind === "land") && native && issue && !proposal ? () => move("Todo") : undefined,
        option: d.kind === "ask" && d.ask ? (i) => d.ask!.options[i] !== undefined && answer(d.ask!.options[i]) : undefined,
        reply: d.kind === "ask" ? () => card.current?.querySelector<HTMLInputElement>(".dd-words input")?.focus() : undefined,
      };
  const finish = () =>
    props.onAct("done", () => invoke("move_issue", { key: issue!.key, state: "Done", evidence: "Done by the human, outside Nitely.", reason: null }));
  const handBack = () => props.onAct("handback", () => invoke("set_owner", { key: issue!.key, owner: "agent" }));
  const [rejecting, setRejecting] = useState(false);
  const [why, setWhy] = useState("");
  const apply = () =>
    props.onAct(
      "apply",
      () => invoke("move_issue", { key: issue!.key, state: "Approved", evidence: null, reason: null }),
      proposal?.in_repo ? "Committed on the integration branch." : "The playbook is changed.",
    );
  const reject = () =>
    why.trim() && props.onAct("reject", () => invoke("move_issue", { key: issue!.key, state: "Canceled", evidence: null, reason: why.trim() }), why.trim());
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
  const status = resolved
    ? DONE_WORD[resolved.move]
    : d.kind === "ask"
      ? "waiting for your answer"
      : d.kind === "own"
        ? "yours to do"
        : live
        ? LIVENESS_LABEL[live]
        : proposal
          ? "proposed a playbook change"
          : "no session open";

  return (
    <article className={`inbox-detail decision-${d.kind}`} ref={card}>
      <div className="dd-context">
        <span className="dd-kind">{proposal ? "Playbook change" : KIND_WORD[d.kind]}</span>
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
        {resolved && (
          <p className={`dd-outcome${resolved.done ? " done" : ""}`} role="status">
            <ResultMark result={resolved.done ? "pass" : "pending"} size={16} />
            {outcomeOf(resolved)}
          </p>
        )}
        {!resolved && d.kind === "ask" && d.ask && (
          <Answer askId={d.ask.id} options={d.ask.options} busy={busy} onAnswer={answer} />
        )}
        {!resolved && pending && proposal && (
          <>
            <div className="actions">
              <button className="btn primary" disabled={busy || !!proposal.stale} onClick={apply}>
                Apply change
              </button>
              <button className="btn" disabled={busy} onClick={() => setRejecting((r) => !r)} aria-expanded={rejecting}>
                Reject…
              </button>
            </div>
            {rejecting && (
              <form
                className="dd-words"
                onSubmit={(e) => {
                  e.preventDefault();
                  reject();
                }}
              >
                <input autoFocus value={why} onChange={(e) => setWhy(e.target.value)} placeholder="Why not? The agent reads this." aria-label="Why the change is rejected" />
                <button className="btn" type="submit" disabled={!why.trim()}>
                  Reject
                </button>
              </form>
            )}
          </>
        )}
        {!resolved && !pending && d.kind === "approve" && issue?.origin === "nitely" && (
          <div className="actions">
            <button className="btn primary" disabled={busy} onClick={() => move("Approved")}>
              Approve
            </button>
            <button className="btn" disabled={busy} onClick={() => move("Todo")}>
              Send back to Todo
            </button>
          </div>
        )}
        {!resolved && d.kind === "own" && issue && (
          <div className="actions">
            <button className="btn primary" disabled={busy} onClick={finish}>
              Mark done
            </button>
            <button className="btn" disabled={busy} onClick={handBack}>
              Hand back to agents
            </button>
          </div>
        )}
        {d.kind === "approve" && issue && issue.origin !== "nitely" && <span className="muted small">In {originName(issue.origin)}. Act on it there.</span>}
        {!resolved && (d.kind === "conflict" || d.kind === "input" || d.kind === "land") && d.row && (
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

/** What the card's line of who and where says once the human acted. */
const DONE_WORD: Record<Move, string> = {
  approve: "approved by you",
  todo: "sent back to Todo",
  answer: "answered",
  done: "done by you",
  handback: "handed back to agents",
  apply: "approved by you",
  reject: "rejected by you",
};

/** The kind of decision as the first word of its card, so a column of them scans at a glance. */
const KIND_WORD: Record<DecisionKind, string> = {
  ask: "Question",
  approve: "Approval",
  own: "Your work",
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
      const proposal = proposalOf(detail);
      if (proposal) {
        return (
          <>
            <p className="decision-matter">
              {who(proposal.created_by)} proposes a change to {proposal.in_repo ? ".nitely/playbook.toml" : "the playbook Nitely keeps"} for {proposal.repo}.
            </p>
            <p className="decision-why pb-why">{proposal.why}</p>
            <PlaybookDiff lines={proposal.diff} />
            {proposal.status !== "pending" ? (
              proposal.outcome && <p className="decision-why pb-applies">{proposal.outcome}</p>
            ) : proposal.stale ? (
              <p className="error small">It no longer applies to the playbook as it is now: {proposal.stale}. Reject it and ask for a new one.</p>
            ) : (
              <p className="decision-why pb-applies">
                {proposal.in_repo ? (
                  <>
                    Approving commits it to <span className="mono">.nitely/playbook.toml</span> on the integration branch it names.
                  </>
                ) : (
                  <>
                    Approving writes it to <span className="mono">{proposal.path}</span>.
                  </>
                )}
              </p>
            )}
          </>
        );
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
                <AgentMark vendor={review.reviewer} size={14} /> {who(review.reviewer)}'s review{review.note ? ` (${review.note})` : ""}{" "}
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
    case "own": {
      // The issue's own words say what to do; the first paragraph is enough here.
      const what = (d.issue?.body ?? "").split(/\n\s*\n/)[0].trim();
      return (
        <>
          <p className="decision-matter">Only you can do this, so no agent will take it.</p>
          {what && <p className="decision-why">{what}</p>}
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
/** Answers being typed, by ask, so picking another decision and coming back keeps the words. */
const drafts = new Map<number, string>();

function Answer(props: { askId: number; options: string[]; busy: boolean; onAnswer: (value: string) => void }) {
  const [text, setTextState] = useState(() => drafts.get(props.askId) ?? "");
  const setText = (value: string) => {
    setTextState(value);
    if (value) drafts.set(props.askId, value);
    else drafts.delete(props.askId);
  };
  return (
    <div className="dd-answer">
      {props.options.map((o, i) => (
        <button key={o} className={`dd-option${i === 0 ? " primary" : ""}`} disabled={props.busy} onClick={() => props.onAnswer(o)} title={i < 9 ? `Answer (${i + 1})` : undefined}>
          <span className="dd-option-n">{i + 1}</span>
          {o}
        </button>
      ))}
      <form
        className="dd-words"
        onSubmit={(e) => {
          e.preventDefault();
          props.onAnswer(text);
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
