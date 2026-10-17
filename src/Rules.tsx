import { useEffect, useMemo, useState } from "react";
import { invoke } from "./backend";
import type { DefaultsView, DryRun, LandBy, PathRule, ReviewerInfo, ReviewFlag, RuleDefaults, RuleField, RulesEdit, RulesView } from "./types";
import { EmptyState, Loading, VENDOR_LABEL, vendorOf } from "./ui";
import "./rules.css";

/** Who lands a reviewed change, in the words the playbook docs use. */
export const LAND_BY: Record<LandBy, { label: string; why: string }> = {
  author: { label: "The agent that wrote it", why: "A passing review moves it to In Review, and the agent calls land (after you approve, when you must)." },
  auto: { label: "Nitely, as soon as it can", why: "Nitely lands it in the same submit when the review passes and nothing needs you, and as soon as you approve it otherwise." },
  human: { label: "Nitely, after you approve", why: "Every change waits for you in Needs You, and Nitely lands it as soon as you approve." },
};

const FROM: Record<RuleField<unknown>["from"], string> = { project: "this project", app: "app default", "built-in": "Nitely's default" };

/** Where a value comes from, beside the value. */
function From(props: { from: RuleField<unknown>["from"] }) {
  return <span className={`rules-from rules-from-${props.from}`}>{FROM[props.from]}</span>;
}

/** "Codex" for "codex", the reviewer name as the playbook has it otherwise. */
function reviewerName(name: string): string {
  const v = vendorOf(name);
  return v === "other" ? name : VENDOR_LABEL[v];
}

/** Path rules as rows of pattern and reason, with add and remove. */
function PathRules(props: { rules: PathRule[]; disabled: boolean; onChange: (rules: PathRule[]) => void; patternHint: string; reasonHint: string }) {
  const set = (i: number, r: Partial<PathRule>) => props.onChange(props.rules.map((x, j) => (j === i ? { ...x, ...r } : x)));
  return (
    <div className="rules-list">
      {props.disabled && props.rules.length === 0 && <p className="muted small rules-none">None.</p>}
      {props.rules.map((r, i) => (
        <div className="rules-item" key={i}>
          <input className="mono" value={r.pattern} disabled={props.disabled} placeholder={props.patternHint} aria-label="Path pattern" onChange={(e) => set(i, { pattern: e.target.value })} />
          <input value={r.reason} disabled={props.disabled} placeholder={props.reasonHint} aria-label="Reason" onChange={(e) => set(i, { reason: e.target.value })} />
          {!props.disabled && (
            <button className="btn btn-quiet" aria-label="Remove this rule" onClick={() => props.onChange(props.rules.filter((_, j) => j !== i))}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!props.disabled && (
        <button className="btn btn-quiet rules-add" onClick={() => props.onChange([...props.rules, { pattern: "", reason: "" }])}>
          Add a path
        </button>
      )}
    </div>
  );
}

function Flags(props: { flags: ReviewFlag[]; disabled: boolean; onChange: (flags: ReviewFlag[]) => void }) {
  const set = (i: number, f: Partial<ReviewFlag>) => props.onChange(props.flags.map((x, j) => (j === i ? { ...x, ...f } : x)));
  return (
    <div className="rules-list">
      {props.disabled && props.flags.length === 0 && <p className="muted small rules-none">None.</p>}
      {props.flags.map((f, i) => (
        <div className="rules-item rules-flag" key={i}>
          <input
            className="mono"
            value={f.name}
            disabled={props.disabled}
            placeholder="MONEY"
            aria-label="Flag name"
            onChange={(e) => set(i, { name: e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, "_") })}
          />
          <input value={f.question} disabled={props.disabled} placeholder="the change alters billing or pricing" aria-label="Yes when" onChange={(e) => set(i, { question: e.target.value })} />
          {!props.disabled && (
            <button className="btn btn-quiet" aria-label="Remove this question" onClick={() => props.onChange(props.flags.filter((_, j) => j !== i))}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!props.disabled && (
        <button className="btn btn-quiet rules-add" onClick={() => props.onChange([...props.flags, { name: "", question: "", reason: "" }])}>
          Add a question
        </button>
      )}
    </div>
  );
}

/** Who reviews whose branches: one row per author, a reviewer for each. */
export function ReviewerMap(props: { map: Record<string, string>; reviewers: ReviewerInfo[]; disabled: boolean; onChange: (map: Record<string, string>) => void }) {
  const names = props.reviewers.map((r) => r.name);
  const rows = Object.keys(props.map).sort((a, b) => (a === "*" ? 1 : b === "*" ? -1 : a.localeCompare(b)));
  return (
    <div className="rules-map">
      {rows.map((author) => (
        <label className="rules-map-row" key={author}>
          <span>{author === "*" ? "Anyone else's" : `${reviewerName(author)}'s`} work</span>
          <select value={props.map[author]} disabled={props.disabled} onChange={(e) => props.onChange({ ...props.map, [author]: e.target.value })}>
            {[...new Set([...names, props.map[author]])].map((n) => {
              const info = props.reviewers.find((r) => r.name === n);
              return (
                <option key={n} value={n}>
                  {reviewerName(n)}
                  {info && !info.installed ? " (not installed)" : ""}
                </option>
              );
            })}
          </select>
        </label>
      ))}
    </div>
  );
}

/** A yes/no setting that can also be left to the level above. */
function Tri(props: { value: boolean | null; inherited: boolean; disabled: boolean; yes: string; no: string; onChange: (v: boolean | null) => void }) {
  return (
    <select
      value={props.value === null ? "inherit" : props.value ? "yes" : "no"}
      disabled={props.disabled}
      onChange={(e) => props.onChange(e.target.value === "inherit" ? null : e.target.value === "yes")}
    >
      <option value="inherit">Inherit ({props.inherited ? props.yes : props.no})</option>
      <option value="yes">{props.yes}</option>
      <option value="no">{props.no}</option>
    </select>
  );
}

/** The edit a view starts from: what the project sets itself, nothing it inherits. */
function draftOf(v: RulesView): RulesEdit {
  const own = <T,>(f: RuleField<T>) => (f.from === "project" ? f.value : null);
  return {
    always_human: v.always_human,
    human_paths: v.human_paths,
    blocked_paths: v.blocked_paths,
    flags: v.flags,
    reviewer: own(v.reviewer),
    require_cross_vendor: own(v.require_cross_vendor),
    land_by: own(v.land_by),
    focus: own(v.focus),
  };
}

/** "Changes wait for you when they touch billing, or ..." for the head of the human card. */
function waitsSummary(d: RulesEdit, landBy: LandBy, crossVendor: boolean): string {
  if (d.always_human || landBy === "human") return "Every change waits for you before it lands.";
  const parts: string[] = [];
  const reasons = [...new Set(d.human_paths.filter((r) => r.pattern.trim()).map((r) => r.reason.trim() || r.pattern.trim()))];
  if (reasons.length) parts.push(`it touches ${reasons.join(", ")}`);
  if (d.flags.length) parts.push(`the reviewer answers yes to ${d.flags.map((f) => f.name || "a question").join(" or ")}`);
  if (crossVendor) parts.push("the reviewer is the author's own vendor");
  return parts.length ? `A change waits for you when ${parts.join(", or when ")}. Anything else lands once its review passes.` : "Nothing waits for you: a change lands once its review passes.";
}

/** One repo's rules: what waits for you, who reviews, who lands, and a dry run. */
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

  return (
    <div className="settings rules">
      <p className="rules-source">
        {v.source === "repo" ? "Committed in " : v.source === "stored" ? "Kept by Nitely in " : "Nitely's defaults; saving writes "}
        <span className="mono">{v.path}</span>
      </p>
      {v.read_only && <p className="rules-readonly">{v.read_only}</p>}

      <section className="settings-card">
        <h2>What waits for you</h2>
        <p className="rules-summary">{waitsSummary(draft, landBy, crossVendor)}</p>
        <label className="rules-check">
          <input type="checkbox" checked={draft.always_human} disabled={ro} onChange={(e) => patch({ always_human: e.target.checked })} />
          Every change waits for me
        </label>
        <h3>Paths that need you</h3>
        <p className="settings-why">A change that touches a matching path waits for you. Patterns are regular expressions over repo paths.</p>
        <PathRules rules={draft.human_paths} disabled={ro} onChange={(human_paths) => patch({ human_paths })} patternHint="^src/billing/" reasonHint="billing and money" />
        <h3>Questions the reviewer answers</h3>
        <p className="settings-why">For what a path cannot show. The reviewer ends with a yes or no for each; a yes sends the change to you.</p>
        <Flags flags={draft.flags} disabled={ro} onChange={(flags) => patch({ flags })} />
        <h3>Paths no task may change</h3>
        <p className="settings-why">Submit and land refuse a branch that touches these.</p>
        <PathRules rules={draft.blocked_paths} disabled={ro} onChange={(blocked_paths) => patch({ blocked_paths })} patternHint="^vendor/" reasonHint="updated by its own process" />
      </section>

      <section className="settings-card">
        <h2>Review</h2>
        <div className="rules-field">
          <div className="rules-field-head">
            <h3>Who reviews whose work</h3>
            <From from={draft.reviewer ? "project" : v.reviewer.inherited_from} />
          </div>
          <ReviewerMap map={draft.reviewer ?? v.reviewer.inherited} reviewers={v.reviewers} disabled={ro} onChange={(reviewer) => patch({ reviewer })} />
          <p className="settings-why small">When the reviewer named here is not installed, another installed vendor reviews, and only then the author's own vendor, in a fresh read-only session.</p>
          {draft.reviewer && !ro && (
            <button className="btn btn-quiet" onClick={() => patch({ reviewer: null })}>
              Use the default
            </button>
          )}
        </div>
        <div className="rules-field">
          <div className="rules-field-head">
            <h3>A review by the author's own vendor</h3>
            <From from={draft.require_cross_vendor !== null ? "project" : v.require_cross_vendor.inherited_from} />
          </div>
          <Tri
            value={draft.require_cross_vendor}
            inherited={v.require_cross_vendor.inherited}
            disabled={ro}
            yes="waits for me"
            no="is enough"
            onChange={(require_cross_vendor) => patch({ require_cross_vendor })}
          />
        </div>
        <div className="rules-field">
          <div className="rules-field-head">
            <h3>What matters most here</h3>
            <From from={draft.focus !== null ? "project" : v.focus.inherited_from} />
          </div>
          <textarea
            rows={3}
            disabled={ro}
            value={draft.focus ?? v.focus.inherited}
            placeholder="Extra instructions for the reviewer, e.g. what is a P0 in this codebase."
            onChange={(e) => patch({ focus: e.target.value })}
          />
          {draft.focus !== null && !ro && (
            <button className="btn btn-quiet" onClick={() => patch({ focus: null })}>
              Use the default
            </button>
          )}
        </div>
      </section>

      <section className="settings-card">
        <div className="rules-field-head">
          <h2>Who lands a reviewed change</h2>
          <From from={draft.land_by !== null ? "project" : v.land_by.inherited_from} />
        </div>
        <select value={draft.land_by ?? "inherit"} disabled={ro} onChange={(e) => patch({ land_by: e.target.value === "inherit" ? null : (e.target.value as LandBy) })}>
          <option value="inherit">Inherit ({LAND_BY[v.land_by.inherited].label})</option>
          {(Object.keys(LAND_BY) as LandBy[]).map((b) => (
            <option key={b} value={b}>
              {LAND_BY[b].label}
            </option>
          ))}
        </select>
        <p className="settings-why rules-land-why">{LAND_BY[landBy].why}</p>
      </section>

      {!ro && (
        <div className="rules-save">
          {loosens ? (
            <div className="rules-loosen" role="alert">
              <p>
                <strong>This sends fewer changes to you.</strong>
              </p>
              <ul>
                {loosens.map((l) => (
                  <li key={l}>{l}</li>
                ))}
              </ul>
              <div className="settings-row">
                <button className="btn danger" disabled={busy} onClick={() => save(true)}>
                  Loosen and save
                </button>
                <button className="btn" disabled={busy} onClick={() => setLoosens(null)}>
                  Keep editing
                </button>
              </div>
            </div>
          ) : (
            <div className="settings-row">
              <button className="btn primary" disabled={busy || !changed} onClick={() => save(false)}>
                Save rules
              </button>
              {changed && (
                <button className="btn" disabled={busy} onClick={() => setDraft(draftOf(v))}>
                  Discard changes
                </button>
              )}
            </div>
          )}
        </div>
      )}
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}

      <DryRunCard repo={v.repo} stale={changed} />
    </div>
  );
}

/** Pick a branch and see what submit would decide for it under the saved rules. */
function DryRunCard(props: { repo: string; stale: boolean }) {
  const [branches, setBranches] = useState<string[] | null>(null);
  const [branch, setBranch] = useState("");
  const [result, setResult] = useState<DryRun | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setBranches(null);
    setResult(null);
    invoke<string[]>("rules_branches", { repo: props.repo })
      .then((b) => {
        if (!current) return;
        setBranches(b);
        setBranch(b[0] ?? "");
      })
      .catch((e) => current && setError(String(e)));
    return () => {
      current = false;
    };
  }, [props.repo]);

  const run = async () => {
    setError(null);
    try {
      setResult(await invoke<DryRun>("rules_dry_run", { repo: props.repo, branch }));
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-card">
      <h2>Dry run</h2>
      <p className="settings-why">What submit would decide for a branch under the saved rules{props.stale ? " (save your changes first to try them)" : ""}. The review does not run, so its findings and answers are not known.</p>
      {branches === null && !error && <Loading />}
      {branches && branches.length === 0 && <p className="muted small">No branches besides the integration branch.</p>}
      {branches && branches.length > 0 && (
        <div className="settings-row">
          <select className="mono" value={branch} onChange={(e) => setBranch(e.target.value)} aria-label="Branch">
            {branches.map((b) => (
              <option key={b} value={b}>
                {b}
              </option>
            ))}
          </select>
          <button className="btn" disabled={!branch} onClick={run}>
            Try it
          </button>
        </div>
      )}
      {result && (
        <div className="rules-dry">
          <p className="rules-dry-outcome">{result.blocked ?? result.outcome}</p>
          <dl>
            <dt>Changes</dt>
            <dd>
              {result.files.length} file{result.files.length === 1 ? "" : "s"} against <span className="mono">{result.base}</span>
            </dd>
            <dt>Reviewer</dt>
            <dd>{result.reviewer ? `${reviewerName(result.reviewer)}${result.reviewer_note ? ` (${result.reviewer_note})` : ""}` : result.reviewer_error}</dd>
            <dt>Waits for you</dt>
            <dd>{result.reasons.length ? result.reasons.join("; ") : "No rule sends it to you"}</dd>
            {result.flags.length > 0 && (
              <>
                <dt>Also if yes</dt>
                <dd>{result.flags.join("; ")}</dd>
              </>
            )}
          </dl>
        </div>
      )}
      {error && <p className="error small">{error}</p>}
    </section>
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
          <RepoRules view={v} onSaved={(saved) => setViews(views.map((x) => (x.repo === saved.repo ? saved : x)))} />
        </div>
      ))}
    </>
  );
}

/** The app-wide defaults, beside Agent setup: what a project that sets nothing uses. */
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
      <h2>Review and landing defaults</h2>
      <p className="settings-why">What every project uses unless its playbook sets its own. A project's Rules tab shows what it inherits from here.</p>
      <div className="rules-field">
        <div className="rules-field-head">
          <h3>Who reviews whose work</h3>
          <From from={draft.reviewer ? "app" : "built-in"} />
        </div>
        <ReviewerMap map={draft.reviewer ?? view.built_in_reviewer} reviewers={view.reviewers} disabled={false} onChange={(reviewer) => patch({ reviewer })} />
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
        <h3>Who lands a reviewed change</h3>
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
        <div className="rules-loosen" role="alert">
          <p>
            <strong>This sends fewer changes to you</strong> in every project that sets none of these.
          </p>
          <ul>
            {loosens.map((l) => (
              <li key={l}>{l}</li>
            ))}
          </ul>
          <div className="settings-row">
            <button className="btn danger" disabled={busy} onClick={() => save(true)}>
              Loosen and save
            </button>
            <button className="btn" disabled={busy} onClick={() => setLoosens(null)}>
              Keep editing
            </button>
          </div>
        </div>
      ) : (
        <div className="settings-row">
          <button className="btn primary" disabled={busy || !changed} onClick={() => save(false)}>
            Save defaults
          </button>
        </div>
      )}
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
