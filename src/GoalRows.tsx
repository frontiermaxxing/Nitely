import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { GoalDetail } from "./types";
import { goalCounts, goalOrder, targetLine } from "./goals";
import { IssueToken, Meter } from "./ui";
import "./goal-rows.css";

/**
 * The active goals of one project (or of every project, on the Overview), one row each. Renders
 * nothing while there are none, so a project without goals looks as it did.
 */
export function GoalRows(props: { project?: string; version: number; onOpenIssue: (key: string) => void; heading?: boolean }) {
  const [goals, setGoals] = useState<GoalDetail[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    invoke<GoalDetail[]>("goals", { project: props.project ?? null })
      .then((g) => live && (setGoals(g), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.project, props.version]);

  if (error) return <p className="error">{error}</p>;
  if (!goals || goals.length === 0) return null;
  const rows = (
    <div className="goal-rows">
      {goals.map((g) => (
        <GoalRow key={g.goal.id} detail={g} showProject={!props.project} onOpenIssue={props.onOpenIssue} />
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

/** One goal: its title and date, a meter of whose move its issues are, then the issues in station order. */
export function GoalRow(props: { detail: GoalDetail; showProject?: boolean; onOpenIssue: (key: string) => void }) {
  const { goal } = props.detail;
  const placed = goalOrder(props.detail.issues);
  const c = goalCounts(placed);
  const total = placed.length;
  // Late only while something is left to do.
  const when = targetLine(goal.target_date, new Date(), c.done === total);
  return (
    <div className="goal-row">
      <div className="goal-head">
        <span className="goal-title" title={goal.outcome}>
          {goal.title}
        </span>
        {props.showProject && <span className="mono goal-project">{goal.project}</span>}
        {when && <span className={`goal-when${when.late ? " goal-late" : ""}`}>{when.text}</span>}
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
