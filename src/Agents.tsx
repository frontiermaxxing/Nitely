import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentPlan, AgentsUpdated } from "./types";

/** Changes still needed by the installed agents. */
export function pendingChanges(agents: AgentPlan[]): number {
  return agents.filter((a) => a.found).flatMap((a) => a.changes.filter((c) => c.action !== "unchanged")).length;
}

/** Each agent's config changes, with the new file behind a disclosure. */
export function AgentChanges(props: { agents: AgentPlan[] }) {
  return (
    <>
      {props.agents.map((agent) => (
        <section key={agent.agent}>
          <h2>
            {agent.agent} {!agent.found && <span className="muted small">not installed, skipped</span>}
          </h2>
          {agent.found &&
            agent.changes.map((c) => (
              <div className="change" key={c.target}>
                <span className={`chip ${c.action}`}>{c.action}</span>
                <div>
                  <div className="small">{c.why}</div>
                  <div className="mono small muted">{c.target}</div>
                  {(c.action === "create" || c.action === "update") && (
                    <details>
                      <summary className="small">Show {c.action === "update" ? "the new file" : "the file"}</summary>
                      <pre className="file">{c.after}</pre>
                    </details>
                  )}
                </div>
              </div>
            ))}
        </section>
      ))}
    </>
  );
}

/** Brings the agents' setup up to this version of Nitely after it changed what it writes. */
export function AgentSetup(props: { agents: AgentPlan[]; onUpdated: () => void }) {
  const [result, setResult] = useState<AgentsUpdated | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pending = pendingChanges(props.agents);

  const update = async () => {
    setBusy(true);
    try {
      setResult(await invoke<AgentsUpdated>("agent_setup_apply"));
      setError(null);
      props.onUpdated();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  return (
    <div className="page">
      <h1>Agent setup</h1>
      <p className="muted">
        Nitely gives your agents a skill, instructions, hooks and its MCP server in your home folder. When Nitely
        changes what it writes there, agents keep the old copy until it is updated here.
      </p>
      {error && <p className="error">{error}</p>}
      {!result && pending === 0 && <p className="small">Every agent's setup is up to date.</p>}
      <AgentChanges agents={props.agents} />
      {!result && pending > 0 && (
        <div className="actions">
          <button className="btn primary" disabled={busy} onClick={update}>
            Make {pending} change{pending === 1 ? "" : "s"}
          </button>
        </div>
      )}
      {result && (
        <section>
          <h2>Done</h2>
          <ul className="small">
            {result.done.map((d) => (
              <li key={d}>{d}</li>
            ))}
          </ul>
          {result.failed.length > 0 && (
            <ul className="small error">
              {result.failed.map((f) => (
                <li key={f}>{f}</li>
              ))}
            </ul>
          )}
        </section>
      )}
    </div>
  );
}
