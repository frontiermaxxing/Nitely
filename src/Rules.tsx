import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { DefaultsView, LandBy, RuleDefaults, RulesEdit, RulesView } from "./types";
import { Button, EmptyState, Loading, RadioCards, SettingsRow, SourceTag, type ChoiceOption } from "./ui";
import { DryRun } from "./rules/DryRun";
import { Landing } from "./rules/Landing";
import { LandsStrip, type ModeFacts, type RulesSection } from "./rules/LandsStrip";
import { Loosen } from "./rules/Loosen";
import { Mode } from "./rules/Mode";
import { Paths } from "./rules/Paths";
import { OwnVendor, ReviewerMap, Reviewers } from "./rules/Reviewers";
import { SaveBar } from "./rules/SaveBar";
import type { RulesDraft } from "./rules/shared";
import { draftChanges, draftOf, LAND_BY } from "./rules/shared";
import { confirmLoosening } from "./rules/save";
import { optionsFor, type ProjectReviewMode } from "./reviewModes";
import "./rules.css";

export { LAND_BY } from "./rules/shared";
export { ReviewerMap } from "./rules/Reviewers";

/** One repo's rules: what waits for you, who reviews, who lands, and a dry run. The draft lives here; each section edits it. */
function RepoRules(props: { view: RulesView; mode: ModeFacts | null; onSaved: (v: RulesView) => void }) {
  const v = props.view;
  const page = useRef<HTMLDivElement>(null);
  const [draft, setDraft] = useState<RulesEdit>(() => draftOf(v));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [loosens, setLoosens] = useState<string[] | null>(null);
  useEffect(() => {
    setDraft(draftOf(v));
    setLoosens(null);
  }, [v]);

  const ro = !v.editable;
  const changes = draftChanges(v, draft);
  const changed = changes.length > 0;
  const landBy = draft.land_by ?? v.land_by.inherited;
  const crossVendor = draft.require_cross_vendor ?? v.require_cross_vendor.inherited;
  const patch = (p: Partial<RulesEdit>) => {
    setDraft({ ...draft, ...p });
    setLoosens(null);
    setNote(null);
  };

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const step = await confirmLoosening(
        () => invoke<string[]>("rules_check", { repo: v.repo, edit: draft }),
        (confirm) => invoke<RulesView>("rules_save", { repo: v.repo, edit: draft, confirm }),
        loosens,
      );
      if ("loosens" in step) setLoosens(step.loosens);
      else {
        setLoosens(null);
        setNote("Saved. The next submit and land use these rules.");
        props.onSaved(step.saved);
      }
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const rules: RulesDraft = { view: v, draft, ro, busy, landBy, crossVendor, patch };
  const go = (section: RulesSection) => page.current?.querySelector(`[data-rules-section="${section}"]`)?.scrollIntoView({ behavior: "smooth", block: "start" });
  return (
    <div className="settings rules" ref={page}>
      <LandsStrip view={v} draft={draft} landBy={landBy} crossVendor={crossVendor} mode={props.mode} onGo={go} />
      <Mode view={v} />
      <div data-rules-section="waits">
        <Paths {...rules} />
      </div>
      <div data-rules-section="review">
        <Reviewers {...rules} onSaved={props.onSaved} />
      </div>
      <div data-rules-section="landing">
        <Landing {...rules} />
      </div>

      <DryRun repo={v.repo} stale={changed} />

      {note && <p className="muted small rules-saved" role="status">{note}</p>}
      {!ro && (loosens || changes.length > 0 || error) && (
        <div className="rules-save">
          {loosens ? (
            <Loosen loosens={loosens} confirmLabel="Loosen and save" busy={busy} onConfirm={save} onKeepEditing={() => setLoosens(null)} />
          ) : (
            <SaveBar label="Save rules" changes={changes} busy={busy} onSave={save} onDiscard={() => setDraft(draftOf(v))} />
          )}
          {error && <p className="error small" role="alert">{error}</p>}
        </div>
      )}
    </div>
  );
}

/** The project's Rules tab: each linked repo's review and landing rules. */
export function ProjectRules(props: { projectKey: string }) {
  const [views, setViews] = useState<RulesView[] | null>(null);
  const [mode, setMode] = useState<ModeFacts | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    // The review mode decides whether the gate and the review run; without it the strip shows the playbook's own.
    let current = true;
    setMode(null);
    invoke<ProjectReviewMode>("project_review_mode", { project: props.projectKey })
      .then((m) => current && setMode({ mode: m.mode, options: optionsFor(m.mode, m.presets, m.overrides) }))
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [props.projectKey]);
  useEffect(() => {
    // A late answer for the project left behind must not show (and then save) under this one.
    let current = true;
    setViews(null);
    setError(null);
    invoke<RulesView[]>("rules", { project: props.projectKey })
      .then((v) => current && setViews(v))
      .catch((e) => current && setError(String(e)));
    return () => {
      current = false;
    };
  }, [props.projectKey]);

  if (error) return <p className="error">{error}</p>;
  if (!views) return <Loading />;
  if (views.length === 0)
    return (
      <EmptyState title="No repo is watched for this project.">
        Add its repo from the sidebar, or run <span className="mono">nitely repo add &lt;path&gt;</span>. Its review and landing rules show up here.
      </EmptyState>
    );
  return (
    <>
      {views.map((v) => (
        <div key={v.repo}>
          {views.length > 1 && <h2 className="rules-repo">{v.repo}</h2>}
          <RepoRules view={v} mode={mode} onSaved={(saved) => setViews((current) => current && current.map((x) => (x.repo === saved.repo ? saved : x)))} />
        </div>
      ))}
    </>
  );
}

/** The app-wide defaults, as rows of Settings' Review defaults section: what a project that sets nothing uses. */
export function RuleDefaultsCard() {
  const [view, setView] = useState<DefaultsView | null>(null);
  const [draft, setDraft] = useState<RuleDefaults>({});
  const [loosens, setLoosens] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => {
    invoke<DefaultsView>("rule_defaults")
      .then((v) => {
        setView(v);
        setDraft(v.defaults);
      })
      .catch((e) => setError(String(e)));
  }, []);
  if (error && !view) return <p className="error">{error}</p>;
  if (!view) return <Loading />;

  const patch = (p: Partial<RuleDefaults>) => {
    const next: RuleDefaults = { ...draft, ...p };
    for (const k of Object.keys(next) as (keyof RuleDefaults)[]) if (next[k] === undefined) delete next[k];
    setDraft(next);
    setLoosens(null);
    setNote(null);
  };
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const step = await confirmLoosening(
        () => invoke<string[]>("rule_defaults_check", { defaults: draft }),
        (confirm) => invoke<DefaultsView>("rule_defaults_save", { defaults: draft, confirm }),
        loosens,
      );
      if ("loosens" in step) setLoosens(step.loosens);
      else {
        setView(step.saved);
        setDraft(step.saved.defaults);
        setLoosens(null);
        setNote("Saved. Projects that set none of these use them from the next submit.");
      }
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };
  const DEFAULT_NAMES: Record<keyof RuleDefaults, string> = { reviewer: "Who reviews whose work", require_cross_vendor: "A review by the author's own vendor", land_by: "Who lands a change", focus: "What matters most in a review" };
  const changes = (Object.keys(DEFAULT_NAMES) as (keyof RuleDefaults)[]).filter((k) => JSON.stringify(draft[k]) !== JSON.stringify(view.defaults[k])).map((k) => DEFAULT_NAMES[k]);

  // What applies when the app sets nothing: Nitely's own defaults.
  const builtIn = { require_cross_vendor: false, land_by: "author" as LandBy };
  const tag = (set: boolean) => <SourceTag source={set ? "app" : "nitely"} />;
  const landOptions: ChoiceOption<LandBy>[] = (Object.keys(LAND_BY) as LandBy[]).map((b) => ({ value: b, label: LAND_BY[b].label, why: LAND_BY[b].why, isDefault: b === builtIn.land_by }));

  return (
    <div className="rules-defaults">
      <SettingsRow
        label="Who reviews whose work"
        why="When the named reviewer is not installed, another installed vendor reviews, and only then the author's own vendor, in a fresh read-only session."
        source={tag(!!draft.reviewer)}
      >
        <div className="rules-stack">
          <ReviewerMap map={draft.reviewer ?? view.built_in_reviewer} reviewers={view.reviewers} crossVendor={draft.require_cross_vendor ?? builtIn.require_cross_vendor} disabled={busy} onChange={(reviewer) => patch({ reviewer })} />
          {draft.reviewer && (
            <Button variant="quiet" disabled={busy} onClick={() => patch({ reviewer: undefined })}>
              Use Nitely's
            </Button>
          )}
        </div>
      </SettingsRow>
      <SettingsRow label="A review by the author's own vendor" why="When no other vendor could review, is that enough, or does the change wait for you?" source={tag(draft.require_cross_vendor !== undefined)}>
        <OwnVendor
          value={draft.require_cross_vendor ?? null}
          inherited={builtIn.require_cross_vendor}
          disabled={busy}
          onChange={(v) => patch({ require_cross_vendor: v ?? undefined })}
        />
      </SettingsRow>
      <SettingsRow label="Who lands a change" source={tag(draft.land_by !== undefined)}>
        <div className="rules-land-by">
          <RadioCards
            aria-label="Who lands a change"
            options={landOptions}
            value={draft.land_by ?? builtIn.land_by}
            disabled={busy}
            onChange={(b) => patch({ land_by: b === builtIn.land_by ? undefined : b })}
          />
        </div>
      </SettingsRow>
      <SettingsRow label="What matters most in a review" why="Instructions every reviewer gets, unless a project sets its own." htmlFor="rule-defaults-focus" source={tag(draft.focus !== undefined)}>
        <textarea
          id="rule-defaults-focus"
          className="rules-focus"
          rows={3}
          disabled={busy}
          value={draft.focus ?? ""}
          placeholder="What is a P0 in your code, which rules a change must never break."
          onChange={(e) => patch({ focus: e.target.value.trim() ? e.target.value : undefined })}
        />
      </SettingsRow>
      {(loosens || changes.length > 0 || error) && (
        <div className="rules-save">
          {loosens ? (
            <Loosen lead="in every project that sets none of these." loosens={loosens} confirmLabel="Loosen and save" busy={busy} onConfirm={save} onKeepEditing={() => setLoosens(null)} />
          ) : (
            <SaveBar label="Save defaults" changes={changes} busy={busy} onSave={save} onDiscard={() => setDraft(view.defaults)} />
          )}
          {error && <p className="error small" role="alert">{error}</p>}
        </div>
      )}
      {note && <p className="muted small rules-saved" role="status">{note}</p>}
    </div>
  );
}
