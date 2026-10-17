import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import { crewChanged, crewDraftOf, crewEditOf, type CrewDraft } from "./crew";
import type { CrewSaved, CrewView } from "./types";

/**
 * A project's crew (docs/crew.md): how many agent workers Nitely runs on its Todo issues, which
 * agents, and the limits on them. One card per repo of the project. A playbook Nitely keeps is
 * written in place; a repo that commits its own gets a commit on its integration branch
 * (crates/nitely-core/src/crew_settings.rs). Used in the project's Settings tab, and meant for the
 * app's Settings view too.
 */
export function CrewSettings(props: { projectKey: string; onSaved?: () => void; /** Bumped to scroll the section into view. */ focus?: number }) {
  const [views, setViews] = useState<CrewView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const top = useRef<HTMLDivElement>(null);

  // Once it has something to show, so the scroll lands where the cards are.
  const shown = views != null || error != null;
  useEffect(() => {
    if (props.focus && shown) top.current?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [props.focus, shown]);

  useEffect(() => {
    let live = true;
    setViews(null);
    invoke<CrewView[]>("crew_settings", { project: props.projectKey })
      .then((v) => live && (setViews(v), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.projectKey]);

  if (error) {
    return (
      <div className="crew-cards" ref={top}>
      <section className="settings-card" id="crew">
        <h2>Crew</h2>
        <p className="error small">{error}</p>
      </section>
      </div>
    );
  }
  if (!views) return null;
  if (views.length === 0) {
    return (
      <div className="crew-cards" ref={top}>
      <section className="settings-card" id="crew">
        <h2>Crew</h2>
        <p className="settings-why">A crew works in a repo. Add this project's repo from the sidebar first.</p>
      </section>
      </div>
    );
  }
  return (
    <div className="crew-cards" ref={top}>
      {views.map((v, i) => (
        <CrewCard
          key={v.repo}
          view={v}
          first={i === 0}
          titled={views.length > 1}
          onSaved={(saved) => {
            setViews((all) => all && all.map((x) => (x.repo === saved.repo ? saved : x)));
            props.onSaved?.();
          }}
        />
      ))}
    </div>
  );
}

function CrewCard(props: { view: CrewView; first: boolean; titled: boolean; onSaved: (v: CrewView) => void }) {
  const v = props.view;
  const [draft, setDraft] = useState<CrewDraft>(() => crewDraftOf(v));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  // A save answered after the card showed another repo must not land on it.
  const shown = useRef(v.repo);
  shown.current = v.repo;

  useEffect(() => {
    setDraft(crewDraftOf(v));
    setConfirming(false);
  }, [v]);

  const patch = (p: Partial<CrewDraft>) => {
    setDraft((d) => ({ ...d, ...p }));
    setConfirming(false);
    setNote(null);
  };
  const edit = crewEditOf(draft);
  const invalid = typeof edit === "string" ? edit : null;
  const changed = crewChanged(v, draft);
  const turningOn = v.workers === 0 && draft.workers > 0;

  const save = () => {
    if (typeof edit === "string") return;
    if (turningOn && !confirming) {
      setConfirming(true);
      return;
    }
    const repo = v.repo;
    setBusy(true);
    setError(null);
    invoke<CrewSaved>("crew_save", { repo, edit })
      .then((s) => {
        if (shown.current !== repo) return;
        setNote(s.done);
        setConfirming(false);
        props.onSaved(s.view);
      })
      .catch((e) => shown.current === repo && setError(String(e)))
      .finally(() => setBusy(false));
  };

  // One picked goes last in the preference order.
  const toggleAgent = (name: string, on: boolean) => patch({ agents: on ? [...draft.agents, name] : draft.agents.filter((a) => a !== name) });
  const moveUp = (name: string) => {
    const i = draft.agents.indexOf(name);
    if (i <= 0) return;
    const next = [...draft.agents];
    [next[i - 1], next[i]] = [next[i], next[i - 1]];
    patch({ agents: next });
  };
  // Picked agents in preference order, then the rest.
  const listed = [...draft.agents.map((n) => v.choices.find((c) => c.name === n) ?? { name: n, installed: false }), ...v.choices.filter((c) => !draft.agents.includes(c.name))];

  return (
    <section className="settings-card crew-settings" id={props.first ? "crew" : undefined}>
      <h2>Crew{props.titled ? ` · ${v.repo}` : ""}</h2>
      <p className="settings-why">
        Nitely starts agent workers on this project's Todo issues, one issue each, and they submit and land through the same gate and review as
        any agent. Running workers always finish their issue; the limits below only stop new ones.
      </p>

      <div className="crew-grid">
        <label className="crew-field" htmlFor={`crew-workers-${v.repo}`}>
          <span className="crew-label">Workers at once</span>
          <span className="crew-hint muted">0 is off</span>
        </label>
        <input
          id={`crew-workers-${v.repo}`}
          className="mono crew-number"
          type="number"
          min={0}
          max={8}
          value={draft.workers}
          onChange={(e) => patch({ workers: Math.max(0, Math.min(8, Math.floor(Number(e.target.value) || 0))) })}
        />

        <span className="crew-field">
          <span className="crew-label">Agents</span>
          <span className="crew-hint muted">first picked, first tried</span>
        </span>
        <div className="crew-agents" role="group" aria-label="Agents">
          {listed.map((c) => {
            const picked = draft.agents.includes(c.name);
            const at = draft.agents.indexOf(c.name);
            return (
              <div className="crew-agent" key={c.name}>
                <label className="rules-check">
                  <input type="checkbox" checked={picked} onChange={(e) => toggleAgent(c.name, e.target.checked)} />
                  <span className="mono">{c.name}</span>
                </label>
                {!c.installed && <span className="muted small">not found on this machine</span>}
                {picked && at > 0 && (
                  <button className="btn btn-quiet crew-up" onClick={() => moveUp(c.name)} aria-label={`Try ${c.name} earlier`}>
                    Try first
                  </button>
                )}
              </div>
            );
          })}
        </div>

        <label className="crew-field" htmlFor={`crew-day-${v.repo}`}>
          <span className="crew-label">Starts a day</span>
          <span className="crew-hint muted">workers started per local day</span>
        </label>
        <input
          id={`crew-day-${v.repo}`}
          className="mono crew-number"
          inputMode="numeric"
          placeholder={String(v.default_max_per_day)}
          value={draft.maxPerDay}
          onChange={(e) => patch({ maxPerDay: e.target.value })}
        />

        <label className="crew-field" htmlFor={`crew-budget-${v.repo}`}>
          <span className="crew-label">Token budget</span>
          <span className="crew-hint muted">millions a day, every agent in the repo counted; empty is none</span>
        </label>
        <span className="crew-budget">
          <input
            id={`crew-budget-${v.repo}`}
            className="mono crew-number"
            inputMode="decimal"
            placeholder="none"
            value={draft.budgetMillions}
            onChange={(e) => patch({ budgetMillions: e.target.value })}
          />
          <span className="muted small">M tokens</span>
        </span>

        <span className="crew-field">
          <span className="crew-label">Quiet hours</span>
          <span className="crew-hint muted">local time; no new worker starts</span>
        </span>
        <span className="crew-quiet">
          <label className="rules-check">
            <input type="checkbox" checked={draft.quiet} onChange={(e) => patch({ quiet: e.target.checked })} />
            <span>From</span>
          </label>
          <input type="time" className="mono" value={draft.quietFrom} disabled={!draft.quiet} onChange={(e) => patch({ quietFrom: e.target.value })} aria-label="Quiet from" />
          <span>to</span>
          <input type="time" className="mono" value={draft.quietTo} disabled={!draft.quiet} onChange={(e) => patch({ quietTo: e.target.value })} aria-label="Quiet until" />
        </span>
      </div>

      {!v.has_gate && draft.workers > 0 && (
        <p className="crew-warn attention-ink small" role="alert">
          This repo's playbook has no gate commands, so the crew will not start: an unattended worker with no gate could land anything. Add them
          under <span className="mono">[gate]</span> in <span className="mono">{v.path}</span>.
        </p>
      )}
      {v.pending_proposal && (
        <p className="muted small">
          A playbook change an agent proposed is waiting for you ({v.pending_proposal}). Approving it later applies it on top of what you save here.
        </p>
      )}
      <p className="muted small crew-where">
        {v.source === "repo" ? (
          <>
            Saving commits the change to <span className="mono">.nitely/playbook.toml</span> on <span className="mono">{v.integration}</span>; the next land pushes it.
          </>
        ) : (
          <>
            Saved to the playbook Nitely keeps for {v.repo}: <span className="mono">{v.path}</span>
          </>
        )}
      </p>

      {confirming && (
        <div className="rules-loosen" role="alert">
          <p>
            Turn the crew on? Up to {draft.workers} worker{draft.workers === 1 ? "" : "s"} start on {v.repo}'s Todo issues as soon as the app sees the
            change, and land what passes review without asking you. Each agent's CLI must be signed in on this machine.
          </p>
          <div className="settings-row">
            <button className="btn primary" disabled={busy} onClick={save}>
              Turn it on
            </button>
            <button className="btn" disabled={busy} onClick={() => setConfirming(false)}>
              Not yet
            </button>
          </div>
        </div>
      )}
      {!confirming && (
        <div className="settings-row crew-save">
          <button className="btn primary" disabled={busy || !changed || !!invalid} onClick={save}>
            {turningOn ? "Turn on" : draft.workers === 0 && v.workers > 0 ? "Turn off" : "Save"}
          </button>
          {changed && (
            <button className="btn" disabled={busy} onClick={() => patch(crewDraftOf(v))}>
              Undo changes
            </button>
          )}
          {changed && invalid && <span className="error small">{invalid}</span>}
        </div>
      )}
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
