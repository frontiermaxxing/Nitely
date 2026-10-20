import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";

export interface ProjectYolo {
  project: string;
  enabled: boolean;
  revision: number;
}

export function useProjectYolo(project: string, version: number, onChanged: () => void) {
  const [value, setValue] = useState<ProjectYolo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const current = useRef(project);
  const request = useRef(0);
  current.current = project;
  useEffect(() => {
    let live = true;
    const id = ++request.current;
    setError(null);
    invoke<ProjectYolo>("project_yolo", { project })
      .then((v) => { if (live && id === request.current) setValue(v); })
      .catch((e) => { if (live && id === request.current) { setValue(null); setError(String(e)); } });
    return () => { live = false; };
  }, [project, version]);
  const shown = value?.project === project ? value : null;
  const toggle = async () => {
    if (!shown || busy) return;
    ++request.current;
    setBusy(true);
    setError(null);
    try {
      const saved = await invoke<ProjectYolo>("set_project_yolo", { project, enabled: !shown.enabled, expectedRevision: shown.revision });
      if (current.current === project) { ++request.current; setValue(saved); }
      onChanged();
    } catch (e) {
      if (current.current === project) setError(String(e));
      // A second window may have changed the setting. Reload its revision before another edit.
      try {
        const fresh = await invoke<ProjectYolo>("project_yolo", { project });
        if (current.current === project) { ++request.current; setValue(fresh); }
      } catch { /* Keep the original save error visible. */ }
    } finally {
      setBusy(false);
    }
  };
  return { value: shown, busy, error, toggle };
}

export type ProjectYoloControl = ReturnType<typeof useProjectYolo>;

export function ProjectYoloSettings({ control }: { control: ProjectYoloControl }) {
  return (
    <section className="settings-card">
      <h2>YOLO mode</h2>
      <p className="settings-why">Skip automatic test gates and PR reviews when submitting and merging changes in this project. Applies to every connected repository. Manual checks remain available.</p>
      <label className="toggle">
        <input type="checkbox" checked={control.value?.enabled ?? false} disabled={!control.value || control.busy} onChange={() => void control.toggle()} />
        {control.busy ? "Saving…" : control.value?.enabled ? "YOLO mode on" : "Enable YOLO mode"}
      </label>
      <p className="settings-hint">Changes are marked as unchecked. Turning this off restores each repository’s configured gates and review rules. Required human approvals still apply.</p>
      {!control.value && !control.error && <p className="muted small">Loading project mode…</p>}
      {control.error && <p className="error small" role="alert">{control.error}</p>}
    </section>
  );
}
