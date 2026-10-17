import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { AgentPlan, AgentsUpdated, ConnectChange } from "./types";
import { AgentMark, vendorInfo, vendorOf } from "./ui";
import { RuleDefaultsCard } from "./Rules";
import { UpdatesCard, type UpdateStatus } from "./Update";

/** Changes still needed by the installed agents. */
export function pendingChanges(agents: AgentPlan[]): number {
  return agents.filter((a) => a.found).flatMap((a) => a.changes.filter((c) => c.action !== "unchanged")).length;
}

/** Agents whose setup could not be read, e.g. a config file that is not valid JSON. */
export function setupErrors(agents: AgentPlan[]): number {
  return agents.filter((a) => a.error).length;
}

/** What a change does, as the first word of its row. */
const ACTION_WORD: Record<ConnectChange["action"], string> = {
  unchanged: "unchanged",
  update: "update ready",
  create: "to add",
  run: "to run",
  manual: "needs a look",
};

/** One card per agent: whether its setup is current, then each file Nitely writes for it. */
export function AgentChanges(props: { agents: AgentPlan[] }) {
  return (
    <>
      {props.agents.map((agent) => {
        const vendor = vendorOf(agent.agent);
        const name = vendor === "other" ? agent.agent : (vendorInfo(vendor)?.product ?? agent.agent);
        const open = agent.changes.filter((c) => c.action !== "unchanged");
        const manual = open.filter((c) => c.action === "manual").length;
        const tone = agent.error || manual ? "fail" : open.length ? "warn" : "ok";
        const status = agent.error
          ? "Could not read it"
          : manual
            ? `${manual} need${manual === 1 ? "s" : ""} a look`
            : open.length
              ? `${open.length} update${open.length === 1 ? "" : "s"}`
              : "Up to date";
        return (
          <section key={agent.agent} className={`vendor-card${agent.found ? "" : " vendor-missing"}`}>
            <div className="vendor-head">
              <AgentMark vendor={agent.agent} size={22} />
              <span className="vendor-name">{name}</span>
              <span className="vendor-meta">{agent.found ? `${agent.changes.length} thing${agent.changes.length === 1 ? "" : "s"} Nitely sets up` : "not installed, skipped"}</span>
              {agent.found && <span className={`vendor-pill vendor-pill-${tone}`}>{status}</span>}
            </div>
            {agent.error && <p className="error small vendor-error">{agent.error}</p>}
            {agent.found &&
              agent.changes.map((c) => (
                <div className="change" key={c.target}>
                  <span className={`chip ${c.action}`}>{ACTION_WORD[c.action]}</span>
                  <div className="change-what">
                    <div>{c.why}</div>
                    <div className="mono muted">{c.target}</div>
                    {c.action === "manual" && <div className="change-note">Nitely leaves this to you: {c.after}</div>}
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
        );
      })}
    </>
  );
}

/** Brings the agents' setup up to this version of Nitely after it changed what it writes. */
export function AgentSetup(props: {
  agents: AgentPlan[];
  onUpdated: () => void;
  onRecheck: () => void;
  update: UpdateStatus | null;
  onUpdate: (s: UpdateStatus) => void;
}) {
  const [result, setResult] = useState<AgentsUpdated | null>(null);
  // Defender's real-time scan of what builds write, per repo, on Windows (NIT-190).
  const [scanNotes, setScanNotes] = useState<{ repo: string; note: string }[]>([]);
  useEffect(() => {
    invoke<{ repo: string; note: string }[]>("build_scan_notes").then(setScanNotes, () => setScanNotes([]));
  }, []);
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
    <div className="page page-form">
      <div className="page-head">
        <h1>Agent setup</h1>
        {/* After a file was fixed by hand, or once the update ran. */}
        <button
          className="btn head-action"
          disabled={busy}
          onClick={() => {
            setResult(null);
            props.onRecheck();
          }}
        >
          Check again
        </button>
      </div>
      <p className="page-lead">
        Nitely gives your agents a skill, instructions, hooks and its MCP server in your home folder. When Nitely
        changes what it writes there, agents keep the old copy until it is updated here.
      </p>
      {error && <p className="error">{error}</p>}
      {scanNotes.map((n) => (
        <p key={n.repo} className="small muted">
          {scanNotes.length > 1 ? `${n.repo}: ` : ""}
          {n.note}.
        </p>
      ))}
      {!result && pending === 0 && setupErrors(props.agents) === 0 && <p className="small muted">Every agent's setup is up to date.</p>}
      <AgentChanges agents={props.agents} />
      {!result && automatic > 0 && (
        <div className="actions">
          <button className="btn primary" disabled={busy} onClick={update}>
            Make {automatic} change{automatic === 1 ? "" : "s"}
          </button>
        </div>
      )}
      <RuleDefaultsCard />
      <UpdatesCard status={props.update} onStatus={props.onUpdate} />
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
