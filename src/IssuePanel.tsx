import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import { issueSpendLine } from "./spend";
import { laneRunId } from "./lanes";
import type { Goal, IssueDetail, IssueRecord, IssueSpend, LinkedIssue } from "./types";
import { agentLabel, agentTitle, ago, openExternal, PRIORITY } from "./util";
import { IssueJourney, WhoHadIt } from "./IssueJourney";
import { issueDecision, type Move } from "./decisions";
import { batchEvents, issueBatchStation, type BatchEvent } from "./batches";
import { GoalPlanDecision } from "./GoalPlanDecision";
import { PlaybookChangeDecision } from "./PlaybookChangeDecision";
import { MirrorFact } from "./Mirror";
import { typing } from "./Palette";
import { AgentMark, IssueToken, Labels, Loading, PrMark, PriorityMark, StationGlyph, StationTrack, VENDOR_LABEL, trackOf, vendorOf } from "./ui";
import "./issue.css";

const KIND_LABEL: Record<string, string> = {
  issue_created: "filed",
  issue_moved: "moved",
  issue_edited: "edited",
  claimed: "claimed",
  status: "status",
  note: "note",
  handoff: "handoff",
  ask: "asked",
  answer: "answered",
  ask_closed: "ask closed",
  worktree: "started",
  gate_passed: "gate passed",
  gate_failed: "gate failed",
  review_started: "review",
  review_failed: "review failed",
  submit: "submitted",
  landed: "landed",
  land_failed: "land stopped",
  crew_started: "crew started",
  crew_ended: "crew ended",
  lane_run: "build lane",
  review_batch: "review batch",
  shadow_batch: "shadow batch",
  land_queue: "land queue",
};

/** One event of the issue's timeline. */
function EventLine({ event: t, now }: { event: IssueDetail["timeline"][number]; now: number }) {
  return (
    <li className={`tl-${t.kind}`}>
      <span className="tl-mark">
        <AgentMark vendor={t.actor} size={14} title={agentTitle(t.actor, t.place)} />
      </span>
      <div className="tl-meta">
        <span className="tl-kind">{KIND_LABEL[t.kind] ?? t.kind}</span> <span title={agentTitle(t.actor, t.place)}>{agentLabel(t.actor)}</span> · {ago(now, t.ts)}
      </div>
      {t.summary && <div className="tl-text">{t.summary}</div>}
      {crewLog(t) && <CrewLogLink path={crewLog(t)!} />}
      {laneRunId(t) !== null && <LaneLogLink id={laneRunId(t)!} />}
    </li>
  );
}

/** A review batch or land queue run the issue went through, with the gate's log for a land. */
function BatchLine({ line: b, now }: { line: BatchEvent; now: number }) {
  const queue = b.kind === "land_queue";
  return (
    <li className={`tl-${b.kind}${b.tone ? ` tl-${b.tone}` : ""}`}>
      <span className="tl-mark">
        <AgentMark vendor={b.actor} size={14} title={queue ? "Nitely's land queue" : agentTitle(b.actor)} />
      </span>
      <div className="tl-meta">
        <span className="tl-kind">{KIND_LABEL[b.kind]}</span> {queue ? "Nitely" : agentLabel(b.actor)} · {ago(now, b.ts)}
      </div>
      <div className="tl-text">{b.text}</div>
      {b.laneRun !== null && <LaneLogLink id={b.laneRun} label="Open the gate's log" />}
    </li>
  );
}

/** The issue's events and its batches' lines (batches.ts), in time order. */
type TimelineItem = { ts: number; event?: IssueDetail["timeline"][number]; batch?: BatchEvent };
function timelineItems(detail: IssueDetail): TimelineItem[] {
  const items: TimelineItem[] = detail.timeline.map((event) => ({ ts: event.ts, event }));
  for (const batch of batchEvents(detail.issue.key, detail.batches)) items.push({ ts: batch.ts, batch });
  // Stable: events at the same second keep their order, and a batch's line comes after them.
  return items.map((it, i) => ({ it, i })).sort((a, b) => a.it.ts - b.it.ts || a.i - b.i).map((x) => x.it);
}

/** The worker log a crew entry names (docs/crew.md), e.g. `... log: C:\\...\\crew\\NIT\\1-nit-3-codex.log`. */
export function crewLog(t: { kind: string; summary: string | null }): string | null {
  if (t.kind !== "crew_started" && t.kind !== "crew_ended") return null;
  return t.summary?.match(/\blog: (.+\.log)$/i)?.[1] ?? null;
}

/** Opens a crew worker's log; the app refuses anything outside Nitely's crew folder. */
function CrewLogLink({ path }: { path: string }) {
  const [error, setError] = useState<string | null>(null);
  return (
    <div className="tl-text">
      <button className="link" title={path} onClick={() => invoke("open_crew_log", { path }).then(() => setError(null), (e) => setError(String(e)))}>
        Open log
      </button>
      {error && <span className="error small"> {error}</span>}
    </div>
  );
}

/** Opens a build lane run's log, by the run's id; the app finds the log in the run's record. */
function LaneLogLink({ id, label = "Open log" }: { id: number; label?: string }) {
  const [error, setError] = useState<string | null>(null);
  return (
    <div className="tl-text">
      <button className="link" title={`Run ${id}`} onClick={() => invoke("open_lane_log", { id }).then(() => setError(null), (e) => setError(String(e)))}>
        {label}
      </button>
      {error && <span className="error small"> {error}</span>}
    </div>
  );
}

/**
 * The issue's spend in one quiet line under its journey: time from start to land, tokens by model,
 * model calls, review rounds. Tokens come from the agents' own session files, read on open.
 */
function SpendLine(props: { detail: IssueDetail; now: number }) {
  const key = props.detail.issue.key;
  const [spend, setSpend] = useState<IssueSpend | null>(null);
  useEffect(() => {
    let live = true;
    invoke<IssueSpend>("issue_spend", { key })
      .then((s) => live && setSpend(s))
      .catch(() => live && setSpend(null));
    return () => {
      live = false;
    };
  }, [key, props.detail.timeline.length]);
  const line = issueSpendLine(props.detail, spend, props.now);
  if (!line) return null;
  return (
    <p className="spend-line" title="Tokens are new input, cache writes and output, read from the agents' session files. Work done from the main checkout counts toward the issue the session last named.">
      {line}
    </p>
  );
}

/** One issue and its timeline, in a panel over the right side. */
export function IssuePanel(props: {
  issueKey: string;
  now: number;
  version: number;
  onClose: () => void;
  onChanged: () => void;
  onOpenIssue?: (key: string) => void;
  /** Shows a goal's row on its project's page. */
  onOpenGoal?: (project: string, goal: number) => void;
}) {
  const [detail, setDetail] = useState<IssueDetail | null>(null);
  // The goals of the issue's project, closed ones too: its goal, and where it can move.
  const [goals, setGoals] = useState<Goal[] | null>(null);
  const project = detail?.issue.project ?? null;
  useEffect(() => {
    if (!project) return;
    let live = true;
    // Another project's goals never show for this issue, even for a moment.
    setGoals((g) => (g && g.every((x) => x.project === project) ? g : null));
    invoke<Goal[]>("goal_choices", { project }).then(
      (g) => live && setGoals(g),
      () => live && setGoals([]),
    );
    return () => {
      live = false;
    };
  }, [project, props.version]);
  const [error, setError] = useState<string | null>(null);
  // Done needs evidence and Canceled a reason, typed in place.
  const [pending, setPending] = useState<"Done" | "Canceled" | null>(null);
  const [why, setWhy] = useState("");
  // After "m", the next key picks where the issue moves.
  const [moving, setMoving] = useState(false);
  // The sheet takes the focus when it opens, so its keys act on this issue and nothing behind it;
  // closing gives the focus back to where it was.
  const sheet = useRef<HTMLElement>(null);
  useEffect(() => {
    const was = document.activeElement as HTMLElement | null;
    sheet.current?.focus({ preventScroll: true });
    return () => {
      if (was?.isConnected) was.focus({ preventScroll: true });
    };
  }, [props.issueKey]);

  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<IssueDetail>("issue_detail", { key: props.issueKey })
        .then((d) => live && (setDetail(d), setError(null)))
        .catch((e) => live && setError(String(e)));
    load();
    return () => {
      live = false;
    };
  }, [props.issueKey, props.version]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && !e.defaultPrevented && props.onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props.onClose]);

  const move = async (state: string, text?: string) => {
    const evidence = state === "Done" ? text ?? null : null;
    const reason = state === "Canceled" ? text ?? null : null;
    try {
      await invoke("move_issue", { key: props.issueKey, state, evidence, reason });
      setPending(null);
      setWhy("");
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const handBack = async () => {
    try {
      await invoke("set_owner", { key: props.issueKey, owner: "agent" });
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const issue = detail?.issue;
  const track = detail && trackOf(detail.issue, detail.timeline, detail.reviews);
  // The same decision the Inbox shows for it (decisions.ts): a playbook change or goal plan to
  // decide, the human's own work, or a change to approve. Never the own-work card for a proposal.
  const decision = issue && issue.origin === "nitely" ? issueDecision(issue, detail ?? undefined) : null;
  const own = decision === "own";
  const waiting = decision === "approve";
  // A decision's buttons act through the backend; the sheet then reloads to show what came of it.
  // Each decision is keyed by its issue, so one opened after another starts with none of its edits.
  const [acting, setActing] = useState(false);
  const act = (_move: Move, run: () => Promise<unknown>) => {
    setActing(true);
    setError(null);
    run()
      .then(() => props.onChanged())
      .catch((e) => setError(String(e)))
      .finally(() => setActing(false));
  };
  const waitedSince = detail ? [...detail.timeline].reverse().find((e) => e.kind === "submit" || (e.kind === "issue_moved" && /→ Needs You/.test(e.summary ?? ""))) : undefined;
  const review = detail?.reviews[0];
  const pr = detail?.prs[0];
  const lastGate = detail ? [...detail.timeline].reverse().find((e) => e.kind === "gate_passed" || e.kind === "gate_failed") : undefined;
  const native = issue?.origin === "nitely";
  const goal = issue?.goal != null ? goals?.find((g) => g.id === issue.goal) : undefined;
  // Keys act only while the focus is in this sheet: a approve, t back to Todo, m then t, d or c.
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!issue || !native || e.repeat || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
    const key = e.key.toLowerCase();
    if (moving) {
      e.preventDefault();
      setMoving(false);
      if (key === "t" && issue.state !== "Todo") move("Todo");
      else if (key === "d" && issue.state !== "Done") setPending("Done");
      else if (key === "c" && issue.state !== "Canceled") setPending("Canceled");
      return;
    }
    if (key === "a" && waiting) {
      e.preventDefault();
      move("Approved");
    } else if (key === "t" && issue.state !== "Todo") {
      e.preventDefault();
      move("Todo");
    } else if (key === "m") {
      e.preventDefault();
      setMoving(true);
    }
  };
  return (
    <>
      <div className="panel-veil" onMouseDown={props.onClose} aria-hidden="true" />
      <aside className="panel" aria-label={`Issue ${props.issueKey}`} ref={sheet} tabIndex={-1} onKeyDown={onKeyDown} onBlur={() => setMoving(false)}>
        <div className="panel-main">
          <div className="panel-head">
            <span className="panel-crumb">
              {issue?.project && <span>{issue.project}</span>}
              {issue?.project && <span>/</span>}
              {goal && (
                <>
                  {goal.state === "active" && props.onOpenGoal ? (
                    <button
                      className="link panel-goal-link"
                      title={`Goal: ${goal.outcome}`}
                      aria-label={`Show the goal ${goal.title}`}
                      onClick={() => props.onOpenGoal!(goal.project, goal.id)}>
                      {goal.title}
                    </button>
                  ) : (
                    <span className="panel-goal-link" title={`Goal (${goal.state}): ${goal.outcome}`}>
                      {goal.title}
                    </span>
                  )}
                  <span>/</span>
                </>
              )}
              <span className="mono">{props.issueKey}</span>
            </span>
            <button className="btn ghost" onClick={props.onClose} title="Close (Esc)">✕</button>
          </div>
          {error && <p className="error small">{error}</p>}
          {!issue && !error && <Loading />}
          {issue && (
            <>
              <div className="panel-title-row">
                {track && <StationGlyph track={track} size={16} />}
                <h2 className="panel-title">{issue.title}</h2>
              </div>
              {track && (
                <div className="panel-track">
                  <StationTrack key={issue.key} track={track} size="full" now={props.now} />
                </div>
              )}
              {issueBatchStation(issue.key, detail.batches) && <p className="panel-batch small">{capital(issueBatchStation(issue.key, detail.batches)!)}</p>}
              <WhoHadIt detail={detail} now={props.now} />
              <IssueJourney key={issue.key} detail={detail} now={props.now} />
              <SpendLine key={`spend-${issue.key}`} detail={detail} now={props.now} />
              <details className="panel-timeline">
                <summary>
                  Timeline <span className="muted">· {timelineItems(detail).length} event{timelineItems(detail).length === 1 ? "" : "s"}</span>
                </summary>
                <ol className="timeline">
                  {timelineItems(detail).map((item, n) =>
                    item.batch ? <BatchLine key={n} line={item.batch} now={props.now} /> : <EventLine key={n} event={item.event!} now={props.now} />,
                  )}
                  {detail.timeline.length === 0 && <li className="muted small">Nothing recorded yet.</li>}
                </ol>
              </details>
            </>
          )}
        </div>
        {issue && detail && (
          <div className="panel-rail">
            {decision === "playbook" && detail.proposal && (
              <div className="panel-waiting panel-decision">
                <p className="panel-waiting-head">Playbook change{waitedSince && ` · ${ago(props.now, waitedSince.ts).replace(" ago", "")}`}</p>
                <PlaybookChangeDecision key={`pb-${issue.key}-${detail.proposal.id}`} proposal={detail.proposal} issueKey={issue.key} resolved={false} busy={acting} onAct={act} />
              </div>
            )}
            {decision === "goal-plan" && detail.goal_plan && (
              <div className="panel-waiting panel-decision">
                <p className="panel-waiting-head">Goal plan</p>
                <GoalPlanDecision key={`plan-${issue.key}`} plan={detail.goal_plan} issueKey={issue.key} resolved={acting} onAct={act} />
              </div>
            )}
            {own && (
              <div className="panel-waiting">
                <p className="panel-waiting-head">Your own work</p>
                <p className="panel-waiting-why">Only you can do this, so no agent will take it.</p>
                <div className="actions">
                  <button className="btn primary" onClick={() => move("Done", "Done by the human, outside Nitely.")}>Mark done</button>
                  <button className="btn" onClick={handBack}>Hand back to agents</button>
                </div>
              </div>
            )}
            {waiting && issue.origin === "nitely" && (
              <div className="panel-waiting">
                <p className="panel-waiting-head">Waiting on you{waitedSince && ` · ${ago(props.now, waitedSince.ts).replace(" ago", "")}`}</p>
                <p className="panel-waiting-why">
                  {review?.reasons.length ? `${capital(review.reasons.slice(0, 2).join("; "))}.` : "The playbook wants a person to approve before it lands."}
                  {review?.reviewer && review.verdict === "pass" && ` ${VENDOR_LABEL[vendorOf(review.reviewer)]} passed it.`}
                </p>
                <div className="actions">
                  <button className="btn primary" onClick={() => move("Approved")} title="Approve (a)">
                    Approve <kbd>a</kbd>
                  </button>
                  <button className="btn" onClick={() => move("Todo")} title="Back to Todo (t)">
                    Back to Todo <kbd>t</kbd>
                  </button>
                </div>
              </div>
            )}
            <dl className="panel-facts">
              <dt>State</dt>
              <dd>{issue.state}</dd>
              {issue.claimed_by && (
                <>
                  <dt>Agent</dt>
                  <dd>
                    <span title={agentTitle(issue.claimed_by)}>
                      <AgentMark vendor={issue.claimed_by} size={16} /> {agentLabel(issue.claimed_by)}
                    </span>
                  </dd>
                </>
              )}
              {review?.reviewer && (
                <>
                  <dt>Reviewer</dt>
                  <dd>
                    <AgentMark vendor={review.reviewer} size={16} /> {VENDOR_LABEL[vendorOf(review.reviewer)]}
                    {review.note && <span className="muted"> ({review.note})</span>}
                  </dd>
                </>
              )}
              {pr && (
                <>
                  <dt>Branch</dt>
                  <dd className="mono path">{pr.branch}</dd>
                  <dt>Pull request</dt>
                  <dd>
                    <PrMark pr={pr} link /> <span className="muted">{pr.state.toLowerCase()}</span>
                  </dd>
                </>
              )}
              {(issue.kind || issue.priority > 0) && (
                <>
                  <dt>Kind</dt>
                  <dd>
                    {issue.priority > 0 && <PriorityMark priority={issue.priority} />}
                    {[issue.kind, issue.priority > 0 && PRIORITY[issue.priority]].filter(Boolean).join(" · ")}
                  </dd>
                </>
              )}
              {issue.labels.length > 0 && (
                <>
                  <dt>Labels</dt>
                  <dd>
                    <Labels labels={issue.labels} />
                  </dd>
                </>
              )}
              {goals && <GoalFact issue={issue} goals={goals} onChanged={props.onChanged} />}
              {issue.origin === "nitely" && <MirrorFact issueKey={issue.key} version={props.version} />}
              {issue.source && (
                <>
                  <dt>Filed by</dt>
                  <dd>{issue.source}</dd>
                </>
              )}
              {lastGate && (
                <>
                  <dt>Gate</dt>
                  <dd className={lastGate.kind === "gate_failed" ? "error" : "live-ink"} title={lastGate.summary ?? undefined}>
                    {lastGate.kind === "gate_failed" ? "failed" : "passed"} {ago(props.now, lastGate.ts)}
                  </dd>
                </>
              )}
            </dl>
            <Relations
              issueKey={issue.key}
              relations={detail.relations}
              editable={issue.origin === "nitely"}
              onOpenIssue={props.onOpenIssue}
              onChanged={props.onChanged}
            />
            {(issue.url && issue.origin !== "nitely") || issue.external_url ? (
              <div className="facts">
                {issue.url && issue.origin !== "nitely" && (
                  <a href={issue.url} onClick={openExternal(issue.url)}>
                    Open in {issue.origin}
                  </a>
                )}
                {issue.external_url && (
                  <a href={issue.external_url} onClick={openExternal(issue.external_url)}>
                    {externalName(issue.external_url)}
                  </a>
                )}
              </div>
            ) : null}
            {(issue.body || issue.evidence) && <div className="panel-rule" />}
            {issue.body && <p className="body">{issue.body}</p>}
            {issue.evidence && (
              <p className="small">
                <span className="muted">Evidence: </span>
                {issue.evidence}
              </p>
            )}
            {moving && (
              <p className="panel-moving" role="status">
                Move to: <kbd>t</kbd> Todo · <kbd>d</kbd> Done… · <kbd>c</kbd> Cancel… · <kbd>esc</kbd> stay
              </p>
            )}
            {issue.origin === "nitely" && (
              <div className="actions panel-moves">
                {!waiting && issue.state !== "Todo" && <button className="btn" onClick={() => move("Todo")}>Back to Todo</button>}
                {issue.state !== "Done" && <button className="btn" onClick={() => setPending("Done")}>Done…</button>}
                {issue.state !== "Canceled" && <button className="btn" onClick={() => setPending("Canceled")}>Cancel…</button>}
              </div>
            )}
            {pending && (
            <form
              className="answer"
              onSubmit={(e) => {
                e.preventDefault();
                if (why.trim()) move(pending, why.trim());
              }}
            >
              <input
                autoFocus
                value={why}
                onChange={(e) => setWhy(e.target.value)}
                placeholder={pending === "Done" ? "What shows it is done?" : "Why cancel it?"}
              />
              <button className="btn primary" disabled={!why.trim()}>{pending === "Done" ? "Mark done" : "Cancel issue"}</button>
              <button type="button" className="btn ghost" onClick={() => setPending(null)}>Back</button>
            </form>
            )}
          </div>
        )}
      </aside>
    </>
  );
}

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/**
 * The goal the issue serves. For a native issue a picker: another active goal of its project, or
 * none. Nothing shows while the project has no goals.
 */
function GoalFact(props: { issue: IssueRecord; goals: Goal[]; onChanged: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const { issue } = props;
  const current = props.goals.find((g) => g.id === issue.goal);
  // A closed goal stays listed while the issue is in it, so the picker shows where it is.
  const choices = props.goals.filter((g) => g.state === "active" || g.id === issue.goal);
  if (issue.origin !== "nitely") {
    return current ? (
      <>
        <dt>Goal</dt>
        <dd>{current.title}</dd>
      </>
    ) : null;
  }
  if (choices.length === 0) return null;
  const set = (value: string) =>
    invoke("set_issue_goal", { key: issue.key, goal: value ? Number(value) : null }).then(
      () => (setError(null), props.onChanged()),
      (e) => setError(String(e)),
    );
  return (
    <>
      <dt>Goal</dt>
      <dd>
        <select className="panel-goal" value={issue.goal ?? ""} onChange={(e) => set(e.target.value)} aria-label="The goal this issue serves">
          <option value="">None</option>
          {choices.map((g) => (
            <option key={g.id} value={g.id}>
              {g.title}
              {g.state === "active" ? "" : ` (${g.state})`}
            </option>
          ))}
        </select>
        {error && <span className="error small">{error}</span>}
      </dd>
    </>
  );
}

/** "Imported from Linear" for a Linear address, else the host. */
function externalName(url: string): string {
  try {
    const host = new URL(url).hostname.replace(/^www\./, "");
    return host === "linear.app" ? "Imported from Linear" : `From ${host}`;
  } catch {
    return "Where it came from";
  }
}

/** An issue on the other end of a relation, shaped as an issue record so IssueToken can draw it. */
function asIssue(l: LinkedIssue): IssueRecord {
  return {
    key: l.key,
    origin: "nitely",
    project: null,
    title: l.title,
    body: "",
    url: "",
    state: l.state,
    state_type: l.open ? "" : l.state === "Done" ? "completed" : "canceled",
    kind: null,
    source: null,
    priority: 0,
    labels: [],
    evidence: "",
    claimed_by: null,
    created_at: "",
    updated_at: "",
    blocked_by: [],
    blocks: [],
    urgency: 0,
  };
}

/** "Waiting on" and "Blocks", with a way to add or remove a blocker. */
function Relations(props: {
  issueKey: string;
  relations: IssueDetail["relations"];
  editable: boolean;
  onOpenIssue?: (key: string) => void;
  onChanged: () => void;
}) {
  const { blocked_by, blocks } = props.relations;
  const [adding, setAdding] = useState(false);
  const [key, setKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const set = async (blocker: string, blocked: boolean) => {
    try {
      await invoke("set_blocker", { key: props.issueKey, blocker, blocked });
      setError(null);
      setAdding(false);
      setKey("");
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };
  if (blocked_by.length === 0 && blocks.length === 0 && !props.editable) return null;
  return (
    <div className="relations">
      {blocked_by.length > 0 && (
        <div className="relation">
          <span className="relation-label">Waiting on</span>
          <span className="relation-items">
            {blocked_by.map((l) => (
              <span key={l.key} className="relation-item">
                <IssueToken issue={asIssue(l)} title onOpen={props.onOpenIssue} />
                {props.editable && (
                  <button className="btn ghost relation-remove" title={`Stop waiting on ${l.key}`} onClick={() => set(l.key, false)}>
                    ✕
                  </button>
                )}
              </span>
            ))}
          </span>
        </div>
      )}
      {blocks.length > 0 && (
        <div className="relation">
          <span className="relation-label">Blocks</span>
          <span className="relation-items">
            {blocks.map((l) => (
              <span key={l.key} className="relation-item">
                <IssueToken issue={asIssue(l)} title onOpen={props.onOpenIssue} />
              </span>
            ))}
          </span>
        </div>
      )}
      {props.editable &&
        (adding ? (
          <form
            className="answer relation-add"
            onSubmit={(e) => {
              e.preventDefault();
              if (key.trim()) set(key.trim().toUpperCase(), true);
            }}
          >
            <input autoFocus value={key} onChange={(e) => setKey(e.target.value)} placeholder="Key of the issue this waits on, e.g. NIT-12" />
            <button className="btn primary" disabled={!key.trim()}>Add</button>
            <button type="button" className="btn ghost" onClick={() => setAdding(false)}>Back</button>
          </form>
        ) : (
          <button className="link relation-new small" onClick={() => setAdding(true)}>
            Add a blocker
          </button>
        ))}
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
