import { useEffect, useMemo, useState } from "react";
import { invoke } from "./backend";
import type { DefaultsView, LandBy, RuleDefaults, RulesEdit, RulesView } from "./types";
import { EmptyState, Loading } from "./ui";
import { DryRun } from "./rules/DryRun";
import { Landing } from "./rules/Landing";
import { Loosen } from "./rules/Loosen";
import { Mode } from "./rules/Mode";
import { Paths } from "./rules/Paths";
import { ReviewerMap, Reviewers } from "./rules/Reviewers";
import { SaveBar } from "./rules/SaveBar";
import type { RulesDraft } from "./rules/shared";
import { draftOf, From, LAND_BY } from "./rules/shared";
import "./rules.css";

export { LAND_BY } from "./rules/shared";
export { ReviewerMap } from "./rules/Reviewers";

/** One repo's rules: what waits for you, who reviews, who lands, and a dry run. The draft lives here; each section edits it. */
function RepoRules(props: { view: RulesView; onSaved: (v: RulesView) => void }) {
  const v = props.view;
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
  const changed = JSON.stringify(draft) !== JSON.stringify(draftOf(v));
  const landBy = draft.land_by ?? v.land_by.inherited;
  const crossVendor = draft.require_cross_vendor ?? v.require_cross_vendor.inherited;
  const patch = (p: Partial<RulesEdit>) => {
    setDraft({ ...draft, ...p });
    setLoosens(null);
    setNote(null);
  };

  const save = async (confirm: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (!confirm) {
        const loose = await invoke<string[]>("rules_check", { repo: v.repo, edit: draft });
        if (loose.length) {
          setLoosens(loose);
          setBusy(false);
          return;
        }
      }
      const saved = await invoke<RulesView>("rules_save", { repo: v.repo, edit: draft, confirm });
      setLoosens(null);
      setNote("Saved. The next submit and land use these rules.");
      props.onSaved(saved);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const rules: RulesDraft = { view: v, draft, ro, busy, landBy, crossVendor, patch };
  return (
    <div className="settings rules">
      <Mode view={v} />
      <Paths {...rules} />
      <Reviewers {...rules} onSaved={props.onSaved} />
      <Landing {...rules} />

      {!ro && (
        <div className="rules-save">
          {loosens ? (
            <Loosen lead={<strong>This sends fewer changes to you.</strong>} loosens={loosens} busy={busy} onLoosen={() => save(true)} onKeepEditing={() => setLoosens(null)} />
          ) : (
            <SaveBar label="Save rules" busy={busy} changed={changed} onSave={() => save(false)} onDiscard={() => setDraft(draftOf(v))} />
          )}
        </div>
      )}
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}

      <DryRun repo={v.repo} stale={changed} />
    </div>
  );
}

/** The project's Rules tab: each linked repo's review and landing rules. */
export function ProjectRules(props: { projectKey: string }) {
  const [views, setViews] = useState<RulesView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
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
          <RepoRules view={v} onSaved={(saved) => setViews((current) => current && current.map((x) => (x.repo === saved.repo ? saved : x)))} />
        </div>
      ))}
    </>
  );
}

/** The app-wide defaults, in Settings: what a project that sets nothing uses. */
export function RuleDefaultsCard({ titled = true }: { titled?: boolean } = {}) {
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
  const changed = useMemo(() => view !== null && JSON.stringify(draft) !== JSON.stringify(view.defaults), [draft, view]);
  if (error && !view) return <p className="error">{error}</p>;
  if (!view) return <Loading />;

  const patch = (p: Partial<RuleDefaults>) => {
    const next: RuleDefaults = { ...draft, ...p };
    for (const k of Object.keys(next) as (keyof RuleDefaults)[]) if (next[k] === undefined) delete next[k];
    setDraft(next);
    setLoosens(null);
    setNote(null);
  };
  const save = async (confirm: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (!confirm) {
        const loose = await invoke<string[]>("rule_defaults_check", { defaults: draft });
        if (loose.length) {
          setLoosens(loose);
          setBusy(false);
          return;
        }
      }
      const saved = await invoke<DefaultsView>("rule_defaults_save", { defaults: draft, confirm });
      setView(saved);
      setDraft(saved.defaults);
      setLoosens(null);
      setNote("Saved. Projects that set none of these use them from the next submit.");
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  return (
    <section className="settings-card rules-defaults">
      {titled && <h2>Review and landing defaults</h2>}
      <p className="settings-why">What every project uses unless its playbook sets its own. A project's Rules tab shows what it inherits from here.</p>
      <div className="rules-field">
        <div className="rules-field-head">
          <h3>Who reviews whose work</h3>
          <From from={draft.reviewer ? "app" : "built-in"} />
        </div>
        <ReviewerMap map={draft.reviewer ?? view.built_in_reviewer} reviewers={view.reviewers} crossVendor={draft.require_cross_vendor ?? false} disabled={false} onChange={(reviewer) => patch({ reviewer })} />
        {draft.reviewer && (
          <button className="btn btn-quiet" onClick={() => patch({ reviewer: undefined })}>
            Use Nitely's
          </button>
        )}
      </div>
      <div className="rules-field">
        <h3>A review by the author's own vendor</h3>
        <select
          value={draft.require_cross_vendor === undefined ? "inherit" : draft.require_cross_vendor ? "yes" : "no"}
          onChange={(e) => patch({ require_cross_vendor: e.target.value === "inherit" ? undefined : e.target.value === "yes" })}
        >
          <option value="inherit">Nitely's default (is enough)</option>
          <option value="yes">waits for me</option>
          <option value="no">is enough</option>
        </select>
      </div>
      <div className="rules-field">
        <h3>Who lands a change</h3>
        <select value={draft.land_by ?? "inherit"} onChange={(e) => patch({ land_by: e.target.value === "inherit" ? undefined : (e.target.value as LandBy) })}>
          <option value="inherit">Nitely's default ({LAND_BY.author.label})</option>
          {(Object.keys(LAND_BY) as LandBy[]).map((b) => (
            <option key={b} value={b}>
              {LAND_BY[b].label}
            </option>
          ))}
        </select>
        <p className="settings-why small">{LAND_BY[draft.land_by ?? "author"].why}</p>
      </div>
      <div className="rules-field">
        <h3>What matters most in a review</h3>
        <textarea rows={3} value={draft.focus ?? ""} placeholder="Instructions every reviewer gets, unless a project sets its own." onChange={(e) => patch({ focus: e.target.value.trim() ? e.target.value : undefined })} />
      </div>
      {loosens ? (
        <Loosen
          lead={<><strong>This sends fewer changes to you</strong> in every project that sets none of these.</>}
          loosens={loosens}
          busy={busy}
          onLoosen={() => save(true)}
          onKeepEditing={() => setLoosens(null)}
        />
      ) : (
        <SaveBar label="Save defaults" busy={busy} changed={changed} onSave={() => save(false)} />
      )}
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
