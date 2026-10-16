import { useState } from "react";
import { invoke } from "./backend";
import type { AgentPlan, AgentsUpdated } from "./types";
import { VENDOR_LABEL, vendorOf } from "./ui";

/** Changes still needed by the installed agents. */
export function pendingChanges(agents: AgentPlan[]): number {
  return agents.filter((a) => a.found).flatMap((a) => a.changes.filter((c) => c.action !== "unchanged")).length;
}

/** Agents whose setup could not be read, e.g. a config file that is not valid JSON. */
export function setupErrors(agents: AgentPlan[]): number {
  return agents.filter((a) => a.error).length;
}

/** Each agent's config changes, with the new file behind a disclosure. */
export function AgentChanges(props: { agents: AgentPlan[] }) {
  return (
    <>
      {props.agents.map((agent) => (
        <section key={agent.agent}>
          <h2>
            {VENDOR_LABEL[vendorOf(agent.agent)] === "Agent" ? agent.agent : VENDOR_LABEL[vendorOf(agent.agent)]} {!agent.found && <span className="muted small">not installed, skipped</span>}
          </h2>
          {agent.error && <p className="error small">{agent.error}</p>}
          {agent.found &&
            agent.changes.map((c) => (
              <div className="change" key={c.target}>
                <span className={`chip ${c.action}`}>{c.action}</span>
                <div>
                  <div className="small">{c.why}</div>
                  <div className="mono small muted">{c.target}</div>
                  {c.action === "manual" && <div className="small">Nitely leaves this to you: {c.after}</div>}
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
export function AgentSetup(props: { agents: AgentPlan[]; onUpdated: () => void; onRecheck: () => void }) {
  const [result, setResult] = useState<AgentsUpdated | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pending = pendingChanges(props.agents);
  const automatic = props.agents
    .filter((a) => a.found)
    .flatMap((a) => a.changes.filter((c) => c.action !== "unchanged" && c.action !== "manual")).length;

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
      {!result && pending === 0 && setupErrors(props.agents) === 0 && <p className="small">Every agent's setup is up to date.</p>}
      <AgentChanges agents={props.agents} />
      <div className="actions">
        {!result && automatic > 0 && (
          <button className="btn primary" disabled={busy} onClick={update}>
            Make {automatic} change{automatic === 1 ? "" : "s"}
          </button>
        )}
        {/* After a file was fixed by hand, or once the update ran. */}
        <button
          className="btn"
          disabled={busy}
          onClick={() => {
            setResult(null);
            props.onRecheck();
          }}
        >
          Check again
        </button>
      </div>
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
