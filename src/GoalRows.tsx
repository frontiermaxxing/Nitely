import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { GoalDetail, GoalRisk } from "./types";
import { crewOnGoal, finishLine, goalBranchLine, goalCounts, goalOrder, goalWorkFinished, showGoal, riskTone, targetLine } from "./goals";
import { IssueToken, Meter, Switch } from "./ui";
import { ProjectFocus } from "./ProjectFocus";
import "./goal-rows.css";

/**
 * The active goals of one project (or of every project, on the Overview), one row each. Renders
 * Overview renders nothing without goals; project views also expose the work focus control.
 */
/** A goal row to bring into view; `at` tells one request from the next for the same goal. */
export type GoalFocus = { id: number; at: number };

export function GoalRows(props: { project?: string; version: number; onOpenIssue: (key: string) => void; heading?: boolean; focus?: GoalFocus }) {
  const [goals, setGoals] = useState<GoalDetail[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Bumped after the person reorders, to read the goals again.
  const [reordered, setReordered] = useState(0);
  // Completed and dropped goals stay hidden unless asked for (NIT-611).
  const [showCompleted, setShowCompleted] = useState(false);

  useEffect(() => {
    let live = true;
    invoke<GoalDetail[]>("goals", { project: props.project ?? null, includeClosed: true })
      .then((g) => live && (setGoals(g), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.project, props.version, reordered]);

  const reopen = (id: number) => {
    invoke("goal_state", { id, state: "active" })
      .then(() => setReordered((n) => n + 1))
      .catch((e) => setError(String(e)));
  };

  // The crew works a project's goals in this order: the person puts one first (NIT-243).
  const first = (id: number) => {
    if (!props.project) return;
    invoke("goal_order", { project: props.project, ids: [id] })
      .then(() => setReordered((n) => n + 1))
      .catch((e) => setError(String(e)));
  };

  if (error) return <p className="error">{error}</p>;
  if (!goals) return null;
  if (!props.project && goals.length === 0) return null;
  const visible = goals.filter((g) => showGoal(g.goal, showCompleted, props.focus?.id));
  const rows = (
    <>
      {/* The focus picker never offers a completed goal. */}
      {props.project && <ProjectFocus key={props.project} project={props.project} version={props.version} goals={goals.filter((g) => g.goal.state === "active")} />}
      {goals.length > 0 && (
        <div className="goal-section-head">
          <h2>Goals {props.heading && <span className="head-note">how close each outcome is</span>}</h2>
          <Switch checked={showCompleted} onChange={setShowCompleted} label="Show completed" />
        </div>
      )}
      <div className="goal-rows">
        {visible.map((g, n) => (
          <GoalRow
            // A new request for the same goal remounts its row, so the scroll and the outline run again.
            key={g.goal.id === props.focus?.id ? `${g.goal.id}@${props.focus.at}` : g.goal.id}
            detail={g}
            showProject={!props.project}
            focused={g.goal.id === props.focus?.id}
            onOpenIssue={props.onOpenIssue}
            onFirst={props.project && n > 0 && g.goal.state === "active" ? () => first(g.goal.id) : undefined}
            onReopen={g.goal.state !== "active" ? () => reopen(g.goal.id) : undefined}
          />
        ))}
      </div>
    </>
  );
  if (!props.heading) return rows;
  return (
    <section>
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

/** The goal's branch, when its items land into one: green with the next checkpoint, or red. */
function GoalBranchChip(props: { goal: GoalDetail["goal"] }) {
  const line = goalBranchLine(props.goal.branch);
  if (!line) return null;
  const b = props.goal.branch!;
  const cost = b.checkpoints ? ` ${b.landed_items} items have landed through it, in ${b.checkpoints} checkpoint${b.checkpoints === 1 ? "" : "s"} with ${b.full_runs} full run${b.full_runs === 1 ? "" : "s"} and ${b.reviews} review${b.reviews === 1 ? "" : "s"}.` : "";
  return (
    <span className={`goal-branch${line.red ? " goal-late" : ""}`} title={`Its items land into ${b.branch} with item checks; a checkpoint lands the branch into main with one full run and one review.${cost}`}>
      <span className="mono">{b.branch}</span>
      {line.text.slice(b.branch.length)}
    </span>
  );
}

/** One goal: its title and date, a meter of whose move its issues are, then the issues in station order. */
export function GoalRow(props: { detail: GoalDetail; showProject?: boolean; focused?: boolean; onOpenIssue: (key: string) => void; onFirst?: () => void; onReopen?: () => void }) {
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
    <div className={`goal-row${props.focused ? " goal-focus" : ""}${goal.state !== "active" ? " goal-row-closed" : ""}`} ref={row} id={`goal-${goal.id}`}>
      <div className="goal-head">
        <span className="goal-title" title={goal.outcome}>
          {goal.title}
        </span>
        {goal.state === "active" && goalWorkFinished(goal) && <span className="goal-state" title="All cards are finished; the goal outcome is still active.">Cards finished</span>}
        {goal.state !== "active" && <span className="goal-state">{goal.state === "done" ? "Completed" : "Dropped"}</span>}
        {props.onReopen && (
          <button className="link goal-reopen" onClick={props.onReopen} title="Make it active again: agents take its open issues, and it can be the focus">
            Reopen
          </button>
        )}
        {props.showProject && <span className="mono goal-project">{goal.project}</span>}
        {when && <span className={`goal-when${when.late ? " goal-late" : ""}`}>{when.text}</span>}
        {pace && (
          <span className={`goal-when${pace.late ? " goal-late" : ""}`} title={pace.title}>
            {pace.text}
          </span>
        )}
        <CrewOnGoal goal={goal} />
        <GoalBranchChip goal={goal} />
        {props.onFirst && (
          <button className="link goal-first" onClick={props.onFirst} title="The crew uses this order when no current focus is set">
            Rank first
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
