import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { projectSpendWords } from "./spend";
import type { IssueRecord, ProjectSpend } from "./types";
import { agentTitle, nowSecs } from "./util";
import { AgentMark, CountBadge, Labels, Meter, PriorityMark, Sparkline, StationGlyph, phaseOf, trackOf } from "./ui";

const GROUPS = ["Needs You", "In Progress", "In Review", "Approved", "Todo", "Backlog", "Done", "Canceled", "Duplicate"];
const DAYS = 14;

/** The project's issues as a list grouped by state. The project page loads them and owns the header. */
export function Backlog(props: { issues: IssueRecord[]; showClosed: boolean; onOpenIssue: (key: string) => void }) {
  const shown = props.issues.filter((i) => props.showClosed || !isClosed(i));
  return (
    <>
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
                  <Labels labels={i.labels} max={3} />
                  {i.kind && <span className="kind">{i.kind}</span>}
                  <span className="who">{i.claimed_by && <AgentMark vendor={i.claimed_by} title={agentTitle(i.claimed_by)} />}</span>
                </button>
              ))}
            </div>
          </section>
        );
      })}
    </>
  );
}

/** Tokens per day as bars, the window's total, and on hover where they went. */
function SpendPart({ spend }: { spend: ProjectSpend }) {
  const words = projectSpendWords(spend);
  return (
    <div className="summary-part" title={words.title}>
      <Sparkline kind="bars" values={spend.days.map((d) => d.tokens)} width={98} height={18} label={`Tokens per day, last ${spend.days.length} days: ${words.total}`} />
      <span className="legend">
        <span>{words.total}</span>
        {spend.per_landed ? <span className="muted">{words.perLanded}</span> : <span className="muted">last {spend.days.length} days</span>}
      </span>
    </div>
  );
}

export const isClosed = (i: IssueRecord) => i.state_type === "completed" || i.state_type === "canceled";

/** Local midnight today, unix seconds: where the spend's last day starts. */
function localMidnight(): number {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return Math.floor(d.getTime() / 1000);
}

/** The project at a glance: open issues by whose move it is, what landed each day, and the tokens spent. */
export function Summary({ issues, projectKey, version }: { issues: IssueRecord[]; projectKey: string; version: number }) {
  const [spend, setSpend] = useState<ProjectSpend | null>(null);
  useEffect(() => {
    let live = true;
    invoke<ProjectSpend>("project_spend", { project: projectKey, dayStart: localMidnight(), days: DAYS })
      .then((s) => live && setSpend(s))
      .catch(() => live && setSpend(null));
    return () => {
      live = false;
    };
  }, [projectKey, version]);
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
      {spend && spend.tokens > 0 && <SpendPart spend={spend} />}
    </div>
  );
}
