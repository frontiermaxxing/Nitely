import { useCallback, useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Board, NetworkInfo, NewMachine, Project } from "./types";
import { ago } from "./util";
import { AgentMark, Button, Notice, EmptyState, Loading, RemoveButton, Select, SettingsRow, StateDot, TextField, livenessLabel, livenessOf } from "./ui";

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
      {error && <p className="error setting-feedback">{error}</p>}

      {made && (
        <div className="setting-notice" aria-live="polite">
          <Notice tone="warning" lead={`${made.name}'s token, shown only now.`}>
            On {made.name}, run this in a terminal to add this backlog to Claude Code. Copy it before you close this: Nitely keeps only a hash of the token and
            cannot show it again.
          </Notice>
          {made.commands.map((c) => (
            <div className="net-command" key={c}>
              <pre className="file">{c}</pre>
              <Button onClick={() => copy(c)}>{copied === c ? "Copied" : "Copy"}</Button>
            </div>
          ))}
          <div className="setting-actions">
            <Button variant="primary" onClick={() => setMade(null)}>
              Done
            </Button>
          </div>
        </div>
      )}

      <div className="setting-sub">
        <h3>Machines</h3>
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
                    <span className="setting-confirm">
                      Cut {t.name} off?
                      <Button variant="danger" disabled={busy} onClick={() => remove(t.name)}>
                        Remove
                      </Button>
                      <Button variant="quiet" onClick={() => setRemoving(null)}>
                        Keep
                      </Button>
                    </span>
                  ) : (
                    <RemoveButton label={`Remove ${t.name}`} onClick={() => setRemoving(t.name)} />
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>

      <SettingsRow label="Add a machine" why="A name is lowercase letters, digits or dashes. Adding a name that exists gives it a new token and cuts off the old one.">
        <form
          className="setting-inline"
          onSubmit={(e) => {
            e.preventDefault();
            if (validName) add();
          }}
        >
          <TextField className="setting-machine" mono value={name} onChange={(e) => setName(e.target.value)} placeholder="mac" aria-label="Machine name" />
          <Select
            className="setting-project"
            value={project}
            onChange={(v) => setProject(v ?? "")}
            options={[{ value: "", label: "No default project" }, ...props.projects.map((p) => ({ value: p.key, label: p.name, detail: p.key }))]}
            aria-label="Default project"
          />
          <Button type="submit" variant="primary" disabled={busy || !validName}>
            Add
          </Button>
        </form>
      </SettingsRow>

      <SettingsRow
        label="Serving"
        why={
          <>
            Run <span className="mono">nitely serve</span> on this machine to let them in. It answers only this machine's networks, and announces itself, so{" "}
            <span className="mono">nitely discover</span> (or <span className="mono">dns-sd -B _nitely._tcp</span> on a Mac) finds it.
          </>
        }
      >
        {info && info.urls.length > 0 ? (
          <ul className="setting-value mono net-urls">
            {info.urls.map((u) => (
              <li key={u}>{u}</li>
            ))}
          </ul>
        ) : (
          <span className="setting-value muted">Not serving now.</span>
        )}
      </SettingsRow>
    </div>
  );
}
