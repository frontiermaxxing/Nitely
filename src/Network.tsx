import { useCallback, useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Board, NetworkInfo, NewMachine, Project } from "./types";
import { ago } from "./util";
import { AgentMark, EmptyState, Loading, StateDot, livenessLabel, livenessOf } from "./ui";

/**
 * Agents on other machines on this network, reaching this backlog through `nitely serve`. A
 * machine's token is made here, by the person at the app, and shown once: no command an agent
 * runs ever prints or stores one.
 */
export function Network(props: { board: Board | null; projects: Project[]; now: number }) {
  const { board, now } = props;
  const [info, setInfo] = useState<NetworkInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [project, setProject] = useState("");
  const [made, setMade] = useState<NewMachine | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    invoke<NetworkInfo>("network").then(
      (n) => {
        setInfo(n);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, []);
  useEffect(load, [load]);

  const add = async () => {
    setBusy(true);
    try {
      const m = await invoke<NewMachine>("network_add", { name: name.trim().toLowerCase(), project: project || null });
      setMade(m);
      setName("");
      setCopied(null);
      setError(null);
      load();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const remove = async (machine: string) => {
    setBusy(true);
    try {
      await invoke("network_remove", { name: machine });
      setRemoving(null);
      if (made?.name === machine) setMade(null);
      load();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(text);
    } catch {
      setError("Could not copy; select the command and copy it by hand.");
    }
  };

  const sessionsOf = (machine: string) => (board?.remote_sessions ?? []).filter((s) => s.cwd === `remote:${machine}`);
  const validName = /^[a-z0-9-]{1,32}$/.test(name.trim().toLowerCase());

  return (
    <div className="network">
      {error && <p className="error">{error}</p>}

      {made && (
        <section className="settings-card net-made" aria-live="polite">
          <h2>{made.name}'s token, shown only now</h2>
          <p className="small">
            On {made.name}, run this in a terminal to add this backlog to Claude Code. Copy it before you close this:
            Nitely keeps only a hash of the token and cannot show it again.
          </p>
          {made.commands.map((c) => (
            <div className="net-command" key={c}>
              <pre className="file">{c}</pre>
              <button className="btn" onClick={() => copy(c)}>
                {copied === c ? "Copied" : "Copy"}
              </button>
            </div>
          ))}
          <div className="actions">
            <button className="btn primary" onClick={() => setMade(null)}>
              Done
            </button>
          </div>
        </section>
      )}

      <section className="settings-card">
        <h2>Machines</h2>
        {!info && !error && <Loading />}
        {info && info.tokens.length === 0 && <EmptyState title="No machines yet.">Add one below; it gets a token for its agents.</EmptyState>}
        {info && info.tokens.length > 0 && (
          <div className="remote-list">
            {info.tokens.map((t) => {
              const sessions = sessionsOf(t.name);
              const latest = sessions.sort((a, b) => b.last_seen_at - a.last_seen_at)[0] ?? null;
              const live = livenessOf(latest, now);
              return (
                <div className="remote-row" key={t.name}>
                  <AgentMark vendor={latest?.vendor ?? "agent"} state={live} />
                  <span className="remote-who">
                    {t.name} {t.project && <span className="muted mono">{t.project}</span>}
                  </span>
                  <span className="remote-state small">
                    <StateDot state={live} /> {latest ? livenessLabel(latest, now) : "not seen yet"}
                  </span>
                  <span className="remote-doing small">{latest?.last_activity ?? <span className="muted">added {ago(now, t.created_at)}</span>}</span>
                  {removing === t.name ? (
                    <span className="net-confirm small">
                      Cut {t.name} off?{" "}
                      <button className="btn" disabled={busy} onClick={() => remove(t.name)}>
                        Remove
                      </button>{" "}
                      <button className="btn quiet" onClick={() => setRemoving(null)}>
                        Keep
                      </button>
                    </span>
                  ) : (
                    <button className="btn quiet" onClick={() => setRemoving(t.name)}>
                      Remove
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </section>

      <section className="settings-card">
        <h2>Add a machine</h2>
        <form
          className="net-add"
          onSubmit={(e) => {
            e.preventDefault();
            if (validName) add();
          }}
        >
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="mac" aria-label="Machine name" className="mono" />
          <select value={project} onChange={(e) => setProject(e.target.value)} aria-label="Default project">
            <option value="">No default project</option>
            {props.projects.map((p) => (
              <option key={p.key} value={p.key}>
                {p.name} ({p.key})
              </option>
            ))}
          </select>
          <button className="btn primary" disabled={busy || !validName}>
            Add
          </button>
        </form>
        <p className="settings-why net-add-why">
          A name is lowercase letters, digits or dashes. Adding a name that exists gives it a new token and cuts off the old one.
        </p>
      </section>

      <section className="settings-card">
        <h2>Serving</h2>
        <p className="settings-why">
          Run <span className="mono">nitely serve</span> on this machine to let them in. It answers only this machine's
          networks, and announces itself, so <span className="mono">nitely discover</span> (or{" "}
          <span className="mono">dns-sd -B _nitely._tcp</span> on a Mac) finds it.
        </p>
        {info && info.urls.length > 0 && (
          <ul className="small mono net-urls">
            {info.urls.map((u) => (
              <li key={u}>{u}</li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
