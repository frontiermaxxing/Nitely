import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import { crewChanged, crewDraftOf, crewEditOf, type CrewDraft } from "./crew";
import type { CrewSaved, CrewView } from "./types";
import { Button, Checkbox, Notice, SettingsRow, SourceTag, TextField } from "./ui";

/**
 * A project's crew (docs/crew.md): how many agent workers Nitely runs on its Todo issues, which
 * agents, and the limits on them. One card per repo of the project. A playbook Nitely keeps is
 * written in place; a repo that commits its own gets a commit on its integration branch
 * (crates/nitely-core/src/crew_settings.rs). Shown in Settings, under each project's heading.
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
      <div ref={top}>
        <p className="error setting-feedback">{error}</p>
      </div>
    );
  }
  if (!views) return null;
  if (views.length === 0) {
    return (
      <div ref={top}>
        <p className="setting-note">A crew works in a repo. Add this project's repo from the sidebar first.</p>
      </div>
    );
  }
  return (
    <div ref={top}>
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

  const id = (field: string) => `crew-${field}-${v.repo}`;
  return (
    <div className="crew-settings" id={props.first ? "crew" : undefined}>
      {props.titled && <h4 className="crew-repo mono">{v.repo}</h4>}

      <SettingsRow label="Workers at once" why="0 is off." htmlFor={id("workers")}>
        <TextField
          id={id("workers")}
          className="setting-number"
          mono
          type="number"
          min={0}
          max={8}
          value={draft.workers}
          onChange={(e) => patch({ workers: Math.max(0, Math.min(8, Math.floor(Number(e.target.value) || 0))) })}
        />
      </SettingsRow>

      <SettingsRow label="Agents" why="The first picked is tried first.">
        <div className="setting-list">
          {listed.map((c) => {
            const picked = draft.agents.includes(c.name);
            const at = draft.agents.indexOf(c.name);
            return (
              <div className="setting-list-item" key={c.name}>
                <Checkbox checked={picked} onChange={(on) => toggleAgent(c.name, on)} label={<span className="mono">{c.name}</span>} />
                {!c.installed && <span className="setting-unit">not found on this machine</span>}
                {picked && at > 0 && (
                  <Button variant="quiet" onClick={() => moveUp(c.name)} aria-label={`Try ${c.name} earlier`}>
                    Try first
                  </Button>
                )}
              </div>
            );
          })}
        </div>
      </SettingsRow>

      <SettingsRow
        label="Starts a day"
        why="Workers started per local day."
        htmlFor={id("day")}
        source={draft.maxPerDay.trim() ? <SourceTag source="project" onReset={() => patch({ maxPerDay: "" })} /> : <SourceTag source="nitely" />}
      >
        <TextField id={id("day")} className="setting-number" mono inputMode="numeric" placeholder={String(v.default_max_per_day)} value={draft.maxPerDay} onChange={(e) => patch({ maxPerDay: e.target.value })} />
      </SettingsRow>

      <SettingsRow
        label="Token budget"
        why="Millions of tokens a day, every agent in the repo counted. Empty is no budget."
        htmlFor={id("budget")}
        source={draft.budgetMillions.trim() ? <SourceTag source="project" onReset={() => patch({ budgetMillions: "" })} /> : <SourceTag source="nitely" />}
      >
        <span className="setting-inline">
          <TextField id={id("budget")} className="setting-number" mono inputMode="decimal" placeholder="none" value={draft.budgetMillions} onChange={(e) => patch({ budgetMillions: e.target.value })} />
          <span className="setting-unit">M tokens</span>
        </span>
      </SettingsRow>

      <SettingsRow label="Quiet hours" why="Local time. No new worker starts in them.">
        <span className="setting-inline">
          <Checkbox checked={draft.quiet} onChange={(quiet) => patch({ quiet })} label="From" />
          <TextField className="setting-time" mono type="time" value={draft.quietFrom} disabled={!draft.quiet} onChange={(e) => patch({ quietFrom: e.target.value })} aria-label="Quiet from" />
          <span className="setting-unit">to</span>
          <TextField className="setting-time" mono type="time" value={draft.quietTo} disabled={!draft.quiet} onChange={(e) => patch({ quietTo: e.target.value })} aria-label="Quiet until" />
        </span>
      </SettingsRow>

      {!v.has_gate && draft.workers > 0 && (
        <div className="setting-notice">
          <Notice tone="warning" lead="The crew will not start.">
            This repo's playbook has no gate commands, and an unattended worker with no gate could land anything. Add them under{" "}
            <span className="mono">[gate]</span> in <span className="mono">{v.path}</span>.
          </Notice>
        </div>
      )}
      {v.pending_proposal && (
        <div className="setting-notice">
          <Notice tone="neutral" lead="A playbook change waits for you.">
            An agent proposed it ({v.pending_proposal}). Approving it later applies it on top of what you save here.
          </Notice>
        </div>
      )}
      <p className="setting-note">
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

      {confirming ? (
        <div className="setting-notice">
          <Notice tone="warning" lead="Turn the crew on?">
            {draft.workers === 1 ? "A worker starts" : `Up to ${draft.workers} workers start`} on {v.repo}'s Todo issues as soon as the app sees the change, and
            work that passes review lands without asking you. Each agent's CLI must be signed in on this machine.
          </Notice>
          <div className="setting-actions">
            <Button variant="primary" disabled={busy} onClick={save}>
              Turn it on
            </Button>
            <Button disabled={busy} onClick={() => setConfirming(false)}>
              Not yet
            </Button>
          </div>
        </div>
      ) : (
        <div className="setting-actions">
          <Button variant="primary" disabled={busy || !changed || !!invalid} onClick={save}>
            {turningOn ? "Turn on" : draft.workers === 0 && v.workers > 0 ? "Turn off" : "Save"}
          </Button>
          {changed && (
            <Button disabled={busy} onClick={() => patch(crewDraftOf(v))}>
              Undo changes
            </Button>
          )}
          {changed && invalid && <span className="error">{invalid}</span>}
        </div>
      )}
      {note && <p className="muted setting-feedback">{note}</p>}
      {error && <p className="error setting-feedback">{error}</p>}
    </div>
  );
}
