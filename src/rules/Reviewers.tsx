import { useEffect, useState, type ReactNode } from "react";
import { invoke } from "../backend";
import type { ReviewerInfo, ReviewerPreview, ReviewerSaved, ReviewFlag, RuleField, RulesView } from "../types";
import { AgentMark, Button, Segmented, Select, SettingsRow, SettingsSection, SourceTag, Switch, vendorOf, type ChoiceOption, type SelectOption } from "../ui";
import type { RulesDraft } from "./shared";
import { reviewerName, sourceOf } from "./shared";

/**
 * Who reviews whose branches: one row per author, its vendor mark, an arrow, and the reviewer in the
 * shared select. The menu marks a reviewer of the author's own vendor (it waits for you when own-vendor
 * reviews do) and one that is not installed here.
 */
export function ReviewerMap(props: { map: Record<string, string>; reviewers: ReviewerInfo[]; disabled: boolean; crossVendor: boolean; onChange: (map: Record<string, string>) => void }) {
  const names = props.reviewers.map((r) => r.name);
  const rows = Object.keys(props.map).sort((a, b) => (a === "*" ? 1 : b === "*" ? -1 : a.localeCompare(b)));
  return (
    <div className="rules-map">
      {rows.map((author) => {
        const label = `${author === "*" ? "Anyone else" : reviewerName(author)}'s work`;
        const options: SelectOption[] = [...new Set([...names, props.map[author]])].map((n) => {
          const info = props.reviewers.find((r) => r.name === n);
          const absent = info !== undefined && !info.installed;
          const same = author !== "*" && vendorOf(n) !== "other" && vendorOf(n) === vendorOf(author);
          return {
            value: n,
            label: reviewerName(n),
            icon: <AgentMark vendor={n} size={16} />,
            // The current one stays as it is, so a playbook naming an absent reviewer still reads.
            disabled: absent && n !== props.map[author] ? "Not installed on this machine" : undefined,
            detail: absent ? "Not installed on this machine" : same ? (props.crossVendor ? "Same vendor as the author: waits for you" : "Same vendor as the author") : undefined,
            tone: same && props.crossVendor ? "warn" : undefined,
          };
        });
        return (
          <div className="rules-map-row" key={author}>
            <span className="rules-map-author">
              {author !== "*" && <AgentMark vendor={author} size={16} />}
              {label}
            </span>
            <svg className="rules-map-arrow" width="22" height="10" viewBox="0 0 22 10" aria-hidden="true">
              <path d="M1 5h18M15 1.5 19 5l-4 3.5" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            <Select
              className="rules-map-select"
              aria-label={`Reviewer of ${label}`}
              value={props.map[author]}
              options={options}
              disabled={props.disabled}
              onChange={(n) => n && props.onChange({ ...props.map, [author]: n })}
            />
          </div>
        );
      })}
    </div>
  );
}

/** Whether a review by the author's own vendor is enough. Picking the default's side clears the project's value. */
export function OwnVendor(props: { value: boolean | null; inherited: boolean; disabled: boolean; onChange: (v: boolean | null) => void }) {
  const options: ChoiceOption<"no" | "yes">[] = [
    { value: "no", label: "Is enough", isDefault: !props.inherited },
    { value: "yes", label: "Waits for me", isDefault: props.inherited },
  ];
  return (
    <Segmented
      aria-label="A review by the author's own vendor"
      options={options}
      value={(props.value ?? props.inherited) ? "yes" : "no"}
      disabled={props.disabled}
      onChange={(v) => props.onChange((v === "yes") === props.inherited ? null : v === "yes")}
    />
  );
}

function ReviewEnabled(props: { enabled: boolean; source: ReactNode; repo: string; flags: ReviewFlag[]; crossVendor: boolean; disabled: boolean; onChange: (enabled: boolean) => void }) {
  return (
    <SettingsRow
      label="Automated review"
      why="Build and test gates, PR creation and approval rules still apply when off, and so do GitHub and Bitbucket branch protections. Reviewer settings are kept."
      source={props.source}
    >
      <div className="rules-stack">
        <Switch checked={props.enabled} disabled={props.disabled} onChange={props.onChange} label={`${props.enabled ? "On" : "Off"} for ${props.repo}`} />
        {!props.enabled && <p className="rules-note">New and queued reviews skip the reviewer; a review already running finishes. Turning this back on requires review before an unreviewed change can land.</p>}
        {!props.enabled && (props.flags.length > 0 || props.crossVendor) && (
          <div className="rules-note attention-ink">
            <p>These conditions are unevaluated while review is off, so changes still wait for your decision:</p>
            <ul>
              {props.flags.map((flag) => (
                <li key={flag.name}>
                  {flag.name}: {flag.question}
                </li>
              ))}
              {props.crossVendor && <li>A review by another vendor is required.</li>}
            </ul>
          </div>
        )}
      </div>
    </SettingsRow>
  );
}

const FALLBACK = "When the named reviewer is not installed, another installed vendor reviews, and only then the author's own vendor, in a fresh read-only session.";

/** Human review of the exact committed-playbook edit before saving. */
function CommittedReviewer({ view: v, crossVendor, onSaved }: { view: RulesView; crossVendor: boolean; onSaved: (v: RulesView) => void }) {
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
    <ReviewEnabled
      enabled={enabled}
      source={<SourceTag source={sourceOf(enabled === v.review_enabled.value ? v.review_enabled.from : "project")} />}
      repo={v.repo}
      flags={v.flags}
      crossVendor={v.require_cross_vendor.value}
      disabled={busy}
      onChange={(next) => { setEnabled(next); setPreview(null); setNote(null); setError(null); }}
    />
    <SettingsRow
      label="Who reviews whose work"
      why={enabled ? FALLBACK : "Kept while review is off."}
      source={<SourceTag source={sourceOf(reviewer ? "project" : v.reviewer.inherited_from)} onReset={busy || !v.reviewer_editable ? undefined : () => patch(null)} />}
    >
      <div className={enabled ? undefined : "rules-dim"}>
        <ReviewerMap map={reviewer ?? v.reviewer.inherited} reviewers={v.reviewers} crossVendor={crossVendor} disabled={busy || !v.reviewer_editable} onChange={patch} />
      </div>
    </SettingsRow>
    {(changed || note || error) && (
      <div className="rules-commit">
        {preview && <div className="rules-review-preview" role="region" aria-label="Review settings change">
          <p>{preview.destination}</p>
          <pre>{preview.diff}</pre>
        </div>}
        {changed && <div className="settings-row">
          <Button variant="primary" disabled={busy} onClick={act}>{preview ? "Commit review settings" : "Preview review settings"}</Button>
          <Button disabled={busy} onClick={() => { patch(original); setEnabled(v.review_enabled.value); }}>Discard changes</Button>
        </div>}
        {note && <p className="muted small" role="status">{note}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
      </div>
    )}
  </>;
}

/** Review: whether it runs, who reviews whose work, own-vendor reviews and what matters most. */
export function Reviewers({ view: v, draft, ro, busy, crossVendor, patch, onSaved }: RulesDraft & { onSaved: (v: RulesView) => void }) {
  const enabled = draft.review_enabled ?? v.review_enabled.inherited;
  // This project's own value has a Reset that clears it; an inherited one says where it comes from.
  const tag = (set: boolean, inherited: RuleField<unknown>["inherited_from"], reset: () => void) => (
    <SourceTag source={sourceOf(set ? "project" : inherited)} onReset={ro ? undefined : reset} />
  );
  const committed = v.source === "repo";
  return (
    <SettingsSection
      title="Review"
      description={committed ? <>Saving commits these to the playbook on <span className="mono">{v.integration}</span>. It is pushed with the next land.</> : "Another vendor's agent reads every change before it lands."}
    >
      {committed ? (
        <CommittedReviewer view={v} crossVendor={crossVendor} onSaved={onSaved} />
      ) : (
        <>
          <ReviewEnabled
            enabled={enabled}
            source={tag(draft.review_enabled !== null, v.review_enabled.inherited_from, () => patch({ review_enabled: null }))}
            repo={v.repo}
            flags={draft.flags}
            crossVendor={crossVendor}
            disabled={busy || ro}
            onChange={(review_enabled) => patch({ review_enabled })}
          />
          <SettingsRow label="Who reviews whose work" why={enabled ? FALLBACK : "Kept while review is off."} source={tag(draft.reviewer !== null, v.reviewer.inherited_from, () => patch({ reviewer: null }))}>
            <div className={enabled ? undefined : "rules-dim"}>
              <ReviewerMap map={draft.reviewer ?? v.reviewer.inherited} reviewers={v.reviewers} crossVendor={crossVendor} disabled={ro} onChange={(reviewer) => patch({ reviewer })} />
            </div>
          </SettingsRow>
        </>
      )}
      <SettingsRow
        label="A review by the author's own vendor"
        why="When no other vendor could review, is that enough, or does the change wait for you?"
        source={tag(draft.require_cross_vendor !== null, v.require_cross_vendor.inherited_from, () => patch({ require_cross_vendor: null }))}
      >
        <OwnVendor value={draft.require_cross_vendor} inherited={v.require_cross_vendor.inherited} disabled={ro} onChange={(require_cross_vendor) => patch({ require_cross_vendor })} />
      </SettingsRow>
      <SettingsRow
        label="What matters most here"
        why="Extra instructions every reviewer reads for this project."
        htmlFor={`rules-focus-${v.repo}`}
        source={tag(draft.focus !== null, v.focus.inherited_from, () => patch({ focus: null }))}
      >
        <textarea
          id={`rules-focus-${v.repo}`}
          className="rules-focus"
          rows={3}
          disabled={ro}
          value={draft.focus ?? v.focus.inherited}
          placeholder="What is a P0 in this codebase, which rules a change must never break."
          onChange={(e) => patch({ focus: e.target.value })}
        />
      </SettingsRow>
    </SettingsSection>
  );
}
