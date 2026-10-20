import { useEffect, useState } from "react";
import { invoke } from "../backend";
import type { ReviewerInfo, ReviewerPreview, ReviewerSaved, ReviewFlag, RuleField, RulesView } from "../types";
import type { RulesDraft } from "./shared";
import { From, reviewerName } from "./shared";

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

function ReviewEnabled(props: { enabled: boolean; from: RuleField<boolean>["from"]; repo: string; flags: ReviewFlag[]; crossVendor: boolean; disabled: boolean; onChange: (enabled: boolean) => void }) {
  return <div className="rules-field">
    <div className="rules-field-head"><h3>Automated PR review</h3><From from={props.from} /></div>
    <label className="rules-check"><input type="checkbox" checked={props.enabled} disabled={props.disabled} onChange={(e) => props.onChange(e.target.checked)} />Run automated review for {props.repo}</label>
    <p className="settings-why small">Applies to this repository. Build/test gates, PR creation and independent human approval rules still apply. GitHub and Bitbucket branch protections stay in effect. Reviewer settings are kept while off.</p>
    {!props.enabled && <>
      <p className="small">Review disabled for this project. New and queued reviews skip the reviewer; a review already running finishes. Turning this back on requires review before an unreviewed change can land.</p>
      {(props.flags.length > 0 || props.crossVendor) && <div className="small attention-ink"><p>These conditions are unevaluated while review is off, so changes still wait for your decision:</p><ul>
        {props.flags.map((flag) => <li key={flag.name}>{flag.name}: {flag.question}</li>)}
        {props.crossVendor && <li>A review by another vendor is required.</li>}
      </ul></div>}
    </>}
  </div>;
}

/** Human review of the exact committed-playbook edit before saving. */
function CommittedReviewer({ view: v, onSaved }: { view: RulesView; onSaved: (v: RulesView) => void }) {
  const original = v.reviewer.from === "project" ? v.reviewer.value : null;
  const [reviewer, setReviewer] = useState<Record<string, string> | null>(original);
  const [enabled, setEnabled] = useState(v.review_enabled.value);
  const [preview, setPreview] = useState<ReviewerPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => { setReviewer(original); setEnabled(v.review_enabled.value); setPreview(null); }, [v]);
  const changed = enabled !== v.review_enabled.value || JSON.stringify(reviewer) !== JSON.stringify(original);
  const patch = (next: Record<string, string> | null) => { setReviewer(next); setPreview(null); setNote(null); setError(null); };
  const act = async () => {
    setBusy(true);
    setError(null);
    try {
      const edit = { reviewer, enabled: enabled === v.review_enabled.value ? undefined : enabled };
      if (!preview) {
        setPreview(await invoke<ReviewerPreview>("reviewer_settings_preview", { repo: v.repo, edit }));
      } else {
        const saved = await invoke<ReviewerSaved>("reviewer_settings_save", { repo: v.repo, edit, preview });
        setNote(saved.done);
        setPreview(null);
        onSaved(saved.view);
      }
    } catch (e) { setError(String(e)); setPreview(null); }
    setBusy(false);
  };
  return <>
    <ReviewEnabled enabled={enabled} from={enabled === v.review_enabled.value ? v.review_enabled.from : "project"} repo={v.repo} flags={v.flags} crossVendor={v.require_cross_vendor.value} disabled={busy} onChange={(next) => { setEnabled(next); setPreview(null); setNote(null); setError(null); }} />
    <div className="rules-field">
    <div className="rules-field-head"><h3>Who reviews whose work</h3><From from={reviewer ? "project" : v.reviewer.inherited_from} /></div>
    <ReviewerMap map={reviewer ?? v.reviewer.inherited} reviewers={v.reviewers} disabled={busy || !v.reviewer_editable} onChange={patch} />
    <p className="settings-why small">When the named reviewer is unavailable, another installed vendor reviews, then the author's vendor in a fresh read-only session.</p>
    <p className="settings-why small">Saving commits review settings for <strong>{v.repo}</strong> to its playbook on <strong>{v.integration}</strong>. It is pushed with the next land.</p>
    {reviewer && <button className="btn btn-quiet" disabled={busy} onClick={() => patch(null)}>Use the default</button>}
    {preview && <div className="rules-review-preview" role="region" aria-label="Review settings change">
      <p>{preview.destination}</p>
      <pre>{preview.diff}</pre>
    </div>}
    {changed && <div className="settings-row">
      <button className="btn primary" disabled={busy} onClick={act}>{preview ? "Commit review settings" : "Preview review settings"}</button>
      <button className="btn" disabled={busy} onClick={() => { patch(original); setEnabled(v.review_enabled.value); }}>Discard changes</button>
    </div>}
    {note && <p className="muted small" role="status">{note}</p>}
    {error && <p className="error small" role="alert">{error}</p>}
  </div></>;
}

/** Review: whether it runs, who reviews whose work, own-vendor reviews and what matters most. */
export function Reviewers({ view: v, draft, ro, busy, crossVendor, patch, onSaved }: RulesDraft & { onSaved: (v: RulesView) => void }) {
  return (
    <section className="settings-card">
      <h2>Review</h2>
      {v.source === "repo" ? <CommittedReviewer view={v} onSaved={onSaved} /> : <>
        <ReviewEnabled enabled={draft.review_enabled ?? v.review_enabled.inherited} from={draft.review_enabled === null ? v.review_enabled.inherited_from : "project"} repo={v.repo} flags={draft.flags} crossVendor={crossVendor} disabled={busy || ro} onChange={(review_enabled) => patch({ review_enabled })} />
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
      </div></>}
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
  );
}
