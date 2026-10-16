import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { open } from "@tauri-apps/plugin-dialog";
import type { Applied, ConnectPlan } from "./types";
import { AgentChanges, pendingChanges } from "./Agents";

const PLAYBOOK_SOURCE: Record<string, string> = {
  repo: "the repo's own .nitely/playbook.toml",
  existing: "the one Nitely already keeps for it",
  aera: "Aera's workflow (scripts/work), detected",
  generated: "a starting one from the repo's branch and scripts",
};

/** Pick a repo folder, see everything Nitely will set up, then add it. */
export function AddProject(props: { onAdded: (projectKey: string) => void }) {
  const [path, setPath] = useState("");
  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const [plan, setPlan] = useState<ConnectPlan | null>(null);
  const [applied, setApplied] = useState<Applied | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = async (p: string, k: string, n: string) => {
    if (!p.trim()) return;
    setBusy(true);
    try {
      const next = await invoke<ConnectPlan>("connect_plan", { path: p, key: k || null, name: n || null });
      setPlan(next);
      setKey(next.project_key);
      setName(next.project_name);
      setError(null);
    } catch (e) {
      setPlan(null);
      setError(String(e));
    }
    setBusy(false);
  };

  useEffect(() => setApplied(null), [path]);

  const choose = async () => {
    const picked = await open({ directory: true, title: "Choose a repo folder" });
    if (typeof picked === "string") {
      setPath(picked);
      setKey("");
      setName("");
      load(picked, "", "");
    }
  };

  const add = async () => {
    setBusy(true);
    try {
      const result = await invoke<Applied>("connect_apply", { path, key: key || null, name: name || null });
      setApplied(result);
      setError(null);
      if (result.failed.length === 0) props.onAdded(result.project);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const changes = plan ? pendingChanges(plan.agents) : 0;

  return (
    <div className="page">
      <h1>Add a project</h1>
      <p className="muted">
        Point Nitely at a repo you already work in. It creates the project, watches the repo and its worktrees, and
        connects your agents so they take work from Nitely and report back. Nothing is written into the repo.
      </p>

      <form
        className="answer"
        onSubmit={(e) => {
          e.preventDefault();
          load(path, "", "");
        }}
      >
        <input value={path} onChange={(e) => setPath(e.target.value)} placeholder="C:\Development\AeraChromium" />
        <button type="button" className="btn" onClick={choose}>Choose…</button>
        <button className="btn" disabled={busy || !path.trim()}>Look</button>
      </form>
      {error && <p className="error">{error}</p>}

      {plan && (
        <>
          <section>
            <h2>Project</h2>
            <div className="fields">
              <label>
                Key
                <input value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} onBlur={() => load(path, key, name)} className="mono" />
              </label>
              <label>
                Name
                <input value={name} onChange={(e) => setName(e.target.value)} onBlur={() => load(path, key, name)} />
              </label>
            </div>
            <p className="small">
              {plan.project_exists ? "Joins the existing project" : "A new project"} for <span className="mono">{plan.root}</span>.
              Tasks branch from and land on <span className="mono">{plan.integration_branch}</span>.
            </p>
            <p className="small">
              Playbook: {PLAYBOOK_SOURCE[plan.playbook.source] ?? plan.playbook.source}, kept at{" "}
              <span className="mono">{plan.playbook.path}</span>.
            </p>
            {plan.playbook.text && (
              <details>
                <summary className="small">Show the playbook</summary>
                <pre className="file">{plan.playbook.text}</pre>
              </details>
            )}
          </section>

          <AgentChanges agents={plan.agents} />

          {plan.warnings.map((w) => (
            <p key={w} className="error small">{w}</p>
          ))}

          {!applied && (
            <div className="actions">
              <button className="btn primary" disabled={busy || !key.trim()} onClick={add}>
                Add {key || "project"}
                {changes > 0 ? ` and make ${changes} change${changes === 1 ? "" : "s"}` : ""}
              </button>
            </div>
          )}
          {applied && (
            <section>
              <h2>Done</h2>
              <ul className="small">
                {applied.done.map((d) => (
                  <li key={d}>{d}</li>
                ))}
              </ul>
              {applied.failed.length > 0 && (
                <ul className="small error">
                  {applied.failed.map((f) => (
                    <li key={f}>{f}</li>
                  ))}
                </ul>
              )}
            </section>
          )}
        </>
      )}
    </div>
  );
}
