import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { GoalDetail, ProjectFocus as Focus } from "./types";
import { Loading } from "./ui";

/** The project's work selection policy, independent of the goal row's scroll highlight. */
export function ProjectFocus(props: { project: string; version: number; goals: GoalDetail[] }) {
  const [focus, setFocus] = useState<Focus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const request = useRef(0);
  const saving = useRef(false);
  const goals = props.goals.filter(({ goal }) => goal.project === props.project);

  useEffect(() => {
    // Reads that began before a save must never replace its result.
    if (saving.current) return;
    const current = ++request.current;
    invoke<Focus>("project_focus", { project: props.project })
      .then((value) => {
        if (current !== request.current) return;
        setFocus(value);
        setError(null);
      })
      .catch((e) => current === request.current && setError(String(e)));
    return () => { request.current++; };
  }, [props.project, props.version, retry]);

  const save = async (goal: number | null, allowUrgent: boolean) => {
    if (saving.current) return;
    request.current++;
    saving.current = true;
    setBusy(true);
    setError(null);
    try {
      const value = await invoke<Focus>("set_project_focus", { project: props.project, goal, allowUrgent });
      // The component is keyed by project, so a late response cannot affect another project.
      setFocus(value);
    } catch (e) {
      setError(String(e));
    } finally {
      saving.current = false;
      setBusy(false);
    }
  };

  const selected = focus?.goal != null;
  const missing = selected && !goals.some((g) => g.goal.id === focus.goal);
  return (
    <section className="project-focus" aria-label="Current focus" aria-busy={busy}>
      <div className="project-focus-controls">
        <label className="project-focus-choice">
          <span>Current focus</span>
          <select
            aria-label="Project focus"
            value={focus?.goal ?? ""}
            disabled={!focus || busy || (!goals.length && !selected)}
            onChange={(e) => save(e.target.value ? Number(e.target.value) : null, focus?.allow_urgent ?? true)}
          >
            <option value="">All work</option>
            {missing && <option value={focus.goal!}>{focus.goal_title ?? "Previous goal"}{!focus.active ? " (inactive)" : ""}</option>}
            {goals.map(({ goal }) => <option key={goal.id} value={goal.id}>{goal.title}</option>)}
          </select>
        </label>
        {selected && <button className="link" disabled={busy} onClick={() => save(null, focus.allow_urgent)}>Clear focus</button>}
        {busy && <span className="muted small" role="status">Saving…</span>}
      </div>
      {!focus && !error && <Loading />}
      {focus && <>
        <p className={`project-focus-explanation${selected && !focus.active ? " project-focus-paused" : ""}`}>
          {selected
            ? focus.active
              ? "Agents take new work from this goal and its prerequisites. Other work waits unless urgent interruptions are allowed. Work already running continues."
              : "This goal is inactive. New work stays paused until you change or clear the focus. Work already running continues."
            : goals.length
              ? "Choose a goal to direct the crew and other agents without changing issue priorities."
              : "Start a goal to choose where agents work next."}
        </p>
        {selected && <label className="project-focus-urgent">
          <input aria-label="Allow urgent work to interrupt" type="checkbox" checked={focus.allow_urgent} disabled={busy || !focus.active} onChange={(e) => save(focus.goal, e.target.checked)} />
          <span>Allow urgent (P1) work to interrupt{!focus.active ? " when the goal is active" : ""}</span>
        </label>}
      </>}
      {error && <p className="error" role="alert">{error} <button className="link" disabled={busy} onClick={() => setRetry((n) => n + 1)}>Retry</button></p>}
    </section>
  );
}
