import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { IssueRecord, Project } from "./types";
import { agentLabel, nowSecs } from "./util";
import { AgentMark, CountBadge, EmptyState, Meter, PriorityMark, Sparkline, StationGlyph, phaseOf, trackOf } from "./ui";

const GROUPS = ["Needs You", "In Progress", "In Review", "Approved", "Todo", "Backlog", "Done", "Canceled", "Duplicate"];
const DAYS = 14;

export function Backlog(props: {
  project: Project | undefined;
  projectKey: string;
  version: number;
  onOpenIssue: (key: string) => void;
}) {
  const [issues, setIssues] = useState<IssueRecord[] | null>(null);
  const [showClosed, setShowClosed] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Closed issues are always loaded: the header counts what landed lately even while the list
  // hides them.
  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<IssueRecord[]>("issues", { project: props.projectKey, includeClosed: true })
        .then((i) => live && (setIssues(i), setError(null)))
        .catch((e) => live && setError(String(e)));
    load();
    return () => {
      live = false;
    };
  }, [props.projectKey, props.version]);

  const isClosed = (i: IssueRecord) => i.state_type === "completed" || i.state_type === "canceled";
  const shown = issues?.filter((i) => showClosed || !isClosed(i)) ?? [];

  return (
    <div className="page">
      <div className="page-head">
        <h1>
          {props.project?.name ?? props.projectKey} <span className="muted mono">{props.projectKey}</span>
        </h1>
        <label className="toggle">
          <input type="checkbox" checked={showClosed} onChange={(e) => setShowClosed(e.target.checked)} />
          Show closed
        </label>
      </div>
      {issues && issues.length > 0 && <Summary issues={issues} />}
      {error && <p className="error">{error}</p>}
      {issues?.length === 0 && (
        <EmptyState title="No issues yet.">
          Agents file them with the <span className="mono">file_issue</span> tool, or run <span className="mono">nitely issue create</span>.
        </EmptyState>
      )}
      {GROUPS.map((state) => {
        const group = shown.filter((i) => i.state === state);
        if (group.length === 0) return null;
        return (
          <section key={state}>
            <h2>
              {state} <CountBadge count={group.length} tone={state === "Needs You" ? "attention" : "quiet"} />
            </h2>
            <div className="list">
              {group.map((i) => (
                <button key={i.key} className={`list-row${isClosed(i) ? " quiet" : ""}`} onClick={() => props.onOpenIssue(i.key)}>
                  <PriorityMark priority={i.priority} />
                  <StationGlyph track={trackOf(i)} />
                  <span className="mono key">{i.key}</span>
                  <span className="title">{i.title}</span>
                  {i.kind && <span className="kind">{i.kind}</span>}
                  <span className="who">{i.claimed_by && <AgentMark vendor={i.claimed_by} title={agentLabel(i.claimed_by)} />}</span>
                </button>
              ))}
            </div>
          </section>
        );
      })}
    </div>
  );
}

/** The project at a glance: open issues by whose move it is, and what landed each day. */
function Summary({ issues }: { issues: IssueRecord[] }) {
  const open = issues.filter((i) => i.state_type !== "completed" && i.state_type !== "canceled");
  const count = (phase: string) => open.filter((i) => phaseOf(trackOf(i).station) === phase).length;
  const needs = count("human");
  const agents = count("agent");
  const queued = count("queued");

  const today = Math.floor(nowSecs() / 86400);
  const landed = Array.from({ length: DAYS }, () => 0);
  for (const i of issues) {
    if (i.state !== "Done") continue;
    const day = today - Math.floor(Date.parse(i.updated_at) / 86400000);
    if (day >= 0 && day < DAYS) landed[DAYS - 1 - day]++;
  }
  const total = landed.reduce((a, b) => a + b, 0);

  return (
    <div className="summary">
      <div className="summary-part">
        <Meter
          width={180}
          label="Open issues"
          parts={[
            { value: needs, color: "var(--attention)", label: "need you" },
            { value: agents, color: "var(--st-working)", label: "with agents" },
            { value: queued, color: "var(--st-todo)", label: "queued" },
          ]}
        />
        <span className="legend">
          {needs > 0 && <span className="attention-ink">{needs} need you</span>}
          <span>{agents} with agents</span>
          <span className="muted">{queued} queued</span>
        </span>
      </div>
      <div className="summary-part">
        <Sparkline kind="bars" values={landed} width={98} height={18} label={`Landed per day, last ${DAYS} days: ${total}`} />
        <span className="legend">
          <span>{total} landed</span>
          <span className="muted">last {DAYS} days</span>
        </span>
      </div>
    </div>
  );
}
