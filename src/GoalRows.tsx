import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { GoalDetail, GoalRisk } from "./types";
import { crewOnGoal, finishLine, goalCounts, goalOrder, riskTone, targetLine } from "./goals";
import { IssueToken, Meter } from "./ui";
import "./goal-rows.css";

/**
 * The active goals of one project (or of every project, on the Overview), one row each. Renders
 * nothing while there are none, so a project without goals looks as it did.
 */
/** A goal row to bring into view; `at` tells one request from the next for the same goal. */
export type GoalFocus = { id: number; at: number };

export function GoalRows(props: { project?: string; version: number; onOpenIssue: (key: string) => void; heading?: boolean; focus?: GoalFocus }) {
  const [goals, setGoals] = useState<GoalDetail[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Bumped after the person reorders, to read the goals again.
  const [reordered, setReordered] = useState(0);

  useEffect(() => {
    let live = true;
    invoke<GoalDetail[]>("goals", { project: props.project ?? null })
      .then((g) => live && (setGoals(g), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.project, props.version, reordered]);

  // The crew works a project's goals in this order: the person puts one first (NIT-243).
  const first = (id: number) => {
    if (!props.project) return;
    invoke("goal_order", { project: props.project, ids: [id] })
      .then(() => setReordered((n) => n + 1))
      .catch((e) => setError(String(e)));
  };

  if (error) return <p className="error">{error}</p>;
  if (!goals || goals.length === 0) return null;
  const rows = (
    <div className="goal-rows">
      {goals.map((g, n) => (
        <GoalRow
          // A new request for the same goal remounts its row, so the scroll and the outline run again.
          key={g.goal.id === props.focus?.id ? `${g.goal.id}@${props.focus.at}` : g.goal.id}
          detail={g}
          showProject={!props.project}
          focused={g.goal.id === props.focus?.id}
          onOpenIssue={props.onOpenIssue}
          onFirst={props.project && n > 0 ? () => first(g.goal.id) : undefined}
        />
      ))}
    </div>
  );
  if (!props.heading) return rows;
  return (
    <section>
      <h2>
        Goals <span className="head-note">how close each outcome is</span>
      </h2>
      {rows}
    </section>
  );
}

/** The crew on a goal: workers on its issues now, and what landed today. Nothing when neither. */
function CrewOnGoal(props: { goal: GoalDetail["goal"] }) {
  const words = crewOnGoal(props.goal);
  if (!words) return null;
  const budget = props.goal.workers ? `, at most ${props.goal.workers} at once` : "";
  return (
    <span className="goal-crew" title={`Crew workers on this goal now${budget}, and its issues landed since midnight`}>
      {words}
    </span>
  );
}

/** One goal: its title and date, a meter of whose move its issues are, then the issues in station order. */
export function GoalRow(props: { detail: GoalDetail; showProject?: boolean; focused?: boolean; onOpenIssue: (key: string) => void; onFirst?: () => void }) {
  const { goal } = props.detail;
  // Opened from an issue's goal: scrolled to and marked for a moment.
  const row = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (props.focused) row.current?.scrollIntoView({ block: "center" });
  }, [props.focused]);
  const placed = goalOrder(props.detail.issues);
  const c = goalCounts(placed);
  const total = placed.length;
  // Late only while something is left to do.
  const today = new Date();
  const when = targetLine(goal.target_date, today, c.done === total);
  const pace = finishLine(props.detail.finish, goal.target_date, today);
  const risks = props.detail.risks ?? [];
  return (
    <div className={`goal-row${props.focused ? " goal-focus" : ""}`} ref={row} id={`goal-${goal.id}`}>
      <div className="goal-head">
        <span className="goal-title" title={goal.outcome}>
          {goal.title}
        </span>
        {props.showProject && <span className="mono goal-project">{goal.project}</span>}
        {when && <span className={`goal-when${when.late ? " goal-late" : ""}`}>{when.text}</span>}
        {pace && (
          <span className={`goal-when${pace.late ? " goal-late" : ""}`} title={pace.title}>
            {pace.text}
          </span>
        )}
        <CrewOnGoal goal={goal} />
        {props.onFirst && (
          <button className="link goal-first" onClick={props.onFirst} title="The crew works this project's goals in order, top first">
            Work this goal first
          </button>
        )}
        <span className="goal-count" title={`${c.done} of ${total} issues done`}>
          <span className="mono">{c.done}</span>/<span className="mono">{total}</span> done
        </span>
      </div>
      <Meter
        width="100%"
        label={`${goal.title}: its issues`}
        parts={[
          { value: c.done, color: "var(--st-done)", label: "done" },
          { value: c.moving, color: "var(--live)", label: "an agent is moving" },
          { value: c.needs, color: "var(--attention)", label: "waiting on you" },
          { value: c.stuck, color: "var(--fail)", label: "stuck" },
          { value: c.queued, color: "var(--line-strong)", label: "queued" },
        ]}
      />
      {risks.length > 0 && <GoalRisks risks={risks} onOpenIssue={props.onOpenIssue} />}
      {total === 0 ? (
        <p className="muted small">No issues in it yet.</p>
      ) : (
        <div className="goal-issues">
          {placed.map((p) => (
            <IssueToken key={p.issue.key} issue={p.issue} track={p.track} onOpen={props.onOpenIssue} />
          ))}
        </div>
      )}
    </div>
  );
}

/** The goal's open issues that may hold it up, each with why: stuck, waiting on you, many review rounds. */
export function GoalRisks(props: { risks: GoalRisk[]; onOpenIssue: (key: string) => void }) {
  return (
    <ul className="goal-risks" aria-label="At risk">
      <li className="goal-risks-label">At risk</li>
      {props.risks.map((r) => (
        <li key={r.key} className={`goal-risk goal-risk-${riskTone(r.why)}`}>
          <button className="link mono" onClick={() => props.onOpenIssue(r.key)} title={r.title}>
            {r.key}
          </button>{" "}
          <span className="goal-risk-why">{r.why}</span>
        </li>
      ))}
    </ul>
  );
}
