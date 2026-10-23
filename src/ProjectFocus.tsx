import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { GoalDetail, ProjectFocus as Focus } from "./types";
import { Loading, Select, Switch } from "./ui";

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
        <div className="project-focus-choice">
          <span>Current focus</span>
          <Select
            aria-label="Project focus"
            value={focus?.goal == null ? "" : String(focus.goal)}
            disabled={!focus || busy || (!goals.length && !selected)}
            onChange={(v) => save(v ? Number(v) : null, focus?.allow_urgent ?? true)}
            options={[
              { value: "", label: "All work" },
              ...(missing ? [{ value: String(focus.goal), label: focus.goal_title ?? "Previous goal", detail: focus.active ? undefined : "Inactive" }] : []),
              ...goals.map(({ goal }) => ({ value: String(goal.id), label: goal.title })),
            ]}
          />
        </div>
        {selected && <button className="link" disabled={busy} onClick={() => save(null, focus.allow_urgent)}>Clear focus</button>}
        {selected && <Switch
          checked={focus.allow_urgent}
          disabled={busy || !focus.active}
          onChange={(on) => save(focus.goal, on)}
          label={`Allow urgent (P1) work to interrupt${!focus.active ? " when the goal is active" : ""}`}
        />}
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
      </>}
      {error && <p className="error" role="alert">{error} <button className="link" disabled={busy} onClick={() => setRetry((n) => n + 1)}>Retry</button></p>}
    </section>
  );
}
