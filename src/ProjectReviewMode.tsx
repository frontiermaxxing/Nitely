import { useEffect, useRef, useState } from "react";
import { invoke, isLive } from "./backend";
import { MODE_DESCRIPTION, MODE_LABEL, editMode, modeSave, modeValidation, optionsFor, type EffectiveRepository, type ModeDraft, type ModeOptions, type PeriodicStatus, type ProjectReviewMode, type ReviewMode } from "./reviewModes";
import "./review-modes.css";

export function useProjectReviewMode(project: string, version: number, onChanged: () => void) {
  const [value, setValue] = useState<ProjectReviewMode | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const current = useRef(project);
  const request = useRef(0);
  current.current = project;
  useEffect(() => {
    let live = true;
    const id = ++request.current;
    setError(null);
    invoke<ProjectReviewMode>("project_review_mode", { project })
      .then((v) => { if (live && id === request.current) setValue(v); })
      .catch((e) => { if (live && id === request.current) { setValue(null); setError(String(e)); } });
    return () => { live = false; };
  }, [project, version]);
  const shown = value?.project === project ? value : null;
  const save = async (draft: ModeDraft) => {
    if (!shown || busy || isLive) return;
    ++request.current;
    setBusy(true);
    setError(null);
    try {
      const saved = await invoke<ProjectReviewMode>("set_project_review_mode", { project, ...modeSave(draft), expectedRevision: shown.revision });
      if (current.current === project) { ++request.current; setValue(saved); }
      onChanged();
    } catch (e) {
      if (current.current === project) setError(`Not saved: ${String(e)}`);
      try {
        const fresh = await invoke<ProjectReviewMode>("project_review_mode", { project });
        if (current.current === project) { ++request.current; setValue(fresh); }
      } catch { /* Keep the save failure visible. */ }
    } finally { setBusy(false); }
  };
  return { value: shown, busy, error, save };
}
export type ReviewModeControl = ReturnType<typeof useProjectReviewMode>;

function Choice({ label, value, choices, onChange }: { label: string; value: string; choices: [string, string][]; onChange: (value: string) => void }) {
  return <label className="review-mode-field"><span>{label}</span><select value={value} onChange={(e) => onChange(e.target.value)}>
    <option value="inherit">Use repository settings</option>
    {choices.map(([v, text]) => <option key={v} value={v}>{text}</option>)}
  </select></label>;
}
function Count({ label, value, min = 1, max, onChange }: { label: string; value: number; min?: number; max?: number; onChange: (n: number) => void }) {
  return <label className="review-mode-field"><span>{label}</span><input type="number" min={min} max={max} step="1" value={Number.isFinite(value) ? value : ""} onChange={(e) => onChange(e.target.value === "" ? NaN : Number(e.target.value))} /></label>;
}
const BOOL: [string, string][] = [["true", "On"], ["false", "Off"]];
const boolValue = (v: boolean | null) => v === null ? "inherit" : String(v);
const fromBool = (v: string) => v === "inherit" ? null : v === "true";

export function ProjectReviewModeSettings({ control, version }: { control: ReviewModeControl; version: number }) {
  const saved = control.value;
  const [draft, setDraft] = useState<ModeDraft | null>(null);
  const custom = useRef<ModeOptions | null>(null);
  const customProject = useRef<string | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  useEffect(() => {
    if (!saved) { setDraft(null); return; }
    if (customProject.current !== saved.project || saved.mode === "custom") custom.current = structuredClone(saved.overrides);
    customProject.current = saved.project;
    setDraft({ mode: saved.mode, overrides: optionsFor(saved.mode, saved.presets, saved.overrides) });
    setLocalError(null);
  }, [saved?.project, saved?.revision]);
  const edit = (change: Partial<ModeOptions>) => {
    if (!draft) return;
    const next = editMode(draft, change);
    custom.current = next.overrides;
    setDraft(next);
  };
  const pick = (mode: ReviewMode) => {
    if (!saved || !draft) return;
    try {
      setDraft({ mode, overrides: optionsFor(mode, saved.presets, custom.current ?? draft.overrides) });
      setLocalError(null);
    } catch (e) { setLocalError(String(e)); }
  };
  const options = draft?.overrides;
  const validation = options ? modeValidation(options) : null;
  const changed = saved && draft && (saved.mode !== draft.mode || (draft.mode === "custom" && JSON.stringify(saved.overrides) !== JSON.stringify(draft.overrides)));
  const periodic = options?.periodic ?? { enabled: true, every_landings: 5, max_delay_seconds: 1800, reviewer: "configured", findings_strategy: "reopen_or_create" };
  const backstop = options?.full_backstop ?? { enabled: true, landings: 5, delay_seconds: 1800 };
  return <section className="settings-card review-mode-settings">
    <div className="review-mode-heading"><h2>Review mode</h2>{saved && <span className="muted small">Current: {MODE_LABEL[saved.mode]}</span>}</div>
    <p className="settings-why">Choose how this project checks changes before and after they reach main. Applies to every connected repository.</p>
    {!saved && !control.error && <p className="muted small">Loading review modes…</p>}
    {saved && draft && options && <>
      <fieldset className="review-mode-presets" disabled={control.busy || isLive} aria-label="Review mode">
        {(Object.keys(MODE_LABEL) as ReviewMode[]).map((mode) => <label key={mode} className={draft.mode === mode ? "review-mode-preset selected" : "review-mode-preset"}>
          <input type="radio" name={`mode-${saved.project}`} value={mode} checked={draft.mode === mode} onChange={() => pick(mode)} />
          <span><strong>{MODE_LABEL[mode]}</strong><small>{MODE_DESCRIPTION[mode]}</small></span>
        </label>)}
      </fieldset>
      <p className="settings-hint">The settings below show your selection. Editing one switches to Custom. “Use repository settings” keeps each repository’s existing rules.</p>
      <fieldset className="review-mode-options" disabled={control.busy || isLive}>
        <legend>Before merging</legend>
        <div className="review-mode-grid">
          <Choice label="Automatic tests" value={options.test_depth ?? "inherit"} choices={[["off", "Off"], ["basic", "Basic checks"], ["risk", "Match the change’s risk"], ["full", "Full suite"]]} onChange={(v) => edit({ test_depth: v === "inherit" ? null : v as ModeOptions["test_depth"] })} />
          <Choice label="PR review" value={options.pre_review ?? "inherit"} choices={[["off", "Off"], ["individual", "Review each change"], ["batched", "Review changes in batches"]]} onChange={(v) => edit({ pre_review: v === "inherit" ? null : v as ModeOptions["pre_review"] })} />
          <Choice label="Batch merges" value={boolValue(options.land_batch)} choices={BOOL} onChange={(v) => edit({ land_batch: fromBool(v) })} />
          <Choice label="Shared build lanes" value={boolValue(options.shared_lanes)} choices={BOOL} onChange={(v) => edit({ shared_lanes: fromBool(v) })} />
        </div>
        <p className="settings-hint">Basic and risk-based checks use the repository’s configured commands. Missing configuration requires the full checks.</p>
      </fieldset>
      <fieldset className="review-mode-options" disabled={control.busy || isLive}>
        <legend>After merging</legend>
        <Choice label="Periodic review of main" value={options.periodic === null ? "inherit" : String(options.periodic.enabled)} choices={BOOL} onChange={(v) => edit({ periodic: v === "inherit" ? null : { ...periodic, enabled: v === "true" } })} />
        {options.periodic?.enabled && <div className="review-mode-grid review-mode-nested">
          <Count label="Review after this many merges" value={periodic.every_landings} max={1000} onChange={(every_landings) => edit({ periodic: { ...periodic, every_landings } })} />
          <Count label="Or after minutes (0 = no time limit)" value={periodic.max_delay_seconds / 60} min={0} max={10080} onChange={(n) => edit({ periodic: { ...periodic, max_delay_seconds: n * 60 } })} />
          <label className="review-mode-field"><span>Periodic reviewer</span><select value={periodic.reviewer} onChange={(e) => edit({ periodic: { ...periodic, reviewer: e.target.value } })}>
            <option value="configured">Repository reviewer routing</option><option value="codex">Codex</option><option value="claude">Claude</option><option value="cursor">Cursor</option>
          </select></label>
          <label className="review-mode-field"><span>When a review finds a problem</span><select value={periodic.findings_strategy} onChange={(e) => edit({ periodic: { ...periodic, findings_strategy: e.target.value } })}>
            <option value="reopen_or_create">Reopen matching card or create follow-up</option><option value="create">Create linked follow-up cards</option>
          </select></label>
        </div>}
        <p className="settings-hint">Periodic reviews check accumulated changes without holding up merges. Failed reviews keep their place so no changes are silently skipped.</p>
        <div className="review-mode-grid">
          <Choice label="Full-suite checks on main" value={options.full_backstop === null ? "inherit" : String(options.full_backstop.enabled)} choices={BOOL} onChange={(v) => edit({ full_backstop: v === "inherit" ? null : { ...backstop, enabled: v === "true" } })} />
          <Choice label="Sampled audits of light changes" value={options.audit_percent === null ? "inherit" : String(options.audit_percent)} choices={[[ "0", "Off"], ["10", "10%"], ["25", "25%"], ["50", "50%"], ["100", "Every eligible change"], ...(![null, 0, 10, 25, 50, 100].includes(options.audit_percent) ? [[String(options.audit_percent), `${options.audit_percent}%`] as [string, string]] : [])]} onChange={(v) => edit({ audit_percent: v === "inherit" ? null : Number(v) })} />
        </div>
        {options.full_backstop?.enabled && <div className="review-mode-grid review-mode-nested">
          <Count label="Full suite after this many light or unchecked merges" value={backstop.landings} max={1000} onChange={(landings) => edit({ full_backstop: { ...backstop, landings } })} />
          <Count label="Full suite after minutes" value={backstop.delay_seconds / 60} max={1440} onChange={(n) => edit({ full_backstop: { ...backstop, delay_seconds: n * 60 } })} />
        </div>}
      </fieldset>
      <p className="settings-hint">Manual checks, release requirements and required human approvals remain separate. This controls Nitely’s automatic work; hosted CI has its own workflow settings.</p>
      {isLive && <p className="settings-hint">Live preview is read-only. Change review modes in the desktop app.</p>}
      {validation && <p className="error small" role="alert">{validation}</p>}
      <div className="review-mode-save"><button className="btn primary" disabled={!changed || !!validation || control.busy || isLive} onClick={() => void control.save(draft)}>{control.busy ? "Saving…" : "Save review mode"}</button><span className="muted small">{changed ? "Unsaved changes" : "Saved"}</span></div>
      <EffectiveModes project={saved.project} revision={saved.revision} />
      <PeriodicReviewHistory project={saved.project} version={version} />
    </>}
    {(control.error || localError) && <p className="error small" role="alert">{control.error || localError}</p>}
  </section>;
}

function EffectiveModes({ project, revision }: { project: string; revision: number }) {
  const [rows, setRows] = useState<EffectiveRepository[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    invoke<EffectiveRepository[]>("project_review_mode_effective", { project }).then((r) => { if (live) { setRows(r); setError(null); } }).catch((e) => { if (live) setError(String(e)); });
    return () => { live = false; };
  }, [project, revision]);
  const tests = { off: "Off", basic: "Basic", risk: "Risk-based", full: "Full" };
  const review = { off: "Off", individual: "Each change", batched: "Batched" };
  return <details className="review-mode-effective"><summary>Saved effective settings by repository</summary>
    {error && <p className="error small">{error}</p>}
    {rows?.length === 0 && <p className="muted small">No connected repositories.</p>}
    {rows?.map((r) => <div key={r.repo}><p className="small mono periodic-repo-name">{r.repo}</p>
      <dl><dt>Tests</dt><dd>{r.options.test_depth ? tests[r.options.test_depth] : "Repository settings"}</dd>
        <dt>PR review</dt><dd>{r.options.pre_review ? review[r.options.pre_review] : "Repository settings"}</dd>
        <dt>Merge batches</dt><dd>{r.options.land_batch ? "On" : "Off"}</dd>
        <dt>Shared lanes</dt><dd>{r.options.shared_lanes ? "On" : "Off"}</dd>
        <dt>Periodic review</dt><dd>{r.options.periodic?.enabled ? `Every ${r.options.periodic.every_landings} merges${r.options.periodic.max_delay_seconds ? ` or ${r.options.periodic.max_delay_seconds / 60} minutes` : ""}` : "Off"}</dd>
        <dt>Full-suite checks on main</dt><dd>{r.options.full_backstop?.enabled ? "On" : "Off"}</dd>
        <dt>Audit sampling</dt><dd>{r.options.audit_percent ? `${r.options.audit_percent}% of eligible changes` : "Off"}</dd></dl>
      {r.warnings.map((w, i) => <p key={i} className="settings-hint">{w}</p>)}
    </div>)}
  </details>;
}

function PeriodicReviewHistory({ project, version }: { project: string; version: number }) {
  const [rows, setRows] = useState<PeriodicStatus[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [retrying, setRetrying] = useState<number | null>(null);
  useEffect(() => {
    let live = true;
    invoke<PeriodicStatus[]>("periodic_review_status", { project }).then((r) => { if (live) { setRows(r); setError(null); } }).catch((e) => { if (live) setError(String(e)); });
    return () => { live = false; };
  }, [project, version, refresh]);
  const retry = async (repo: string, id: number) => {
    if (isLive || retrying !== null) return;
    setRetrying(id);
    try { await invoke("retry_periodic_review", { project, repo, id }); setRefresh((n) => n + 1); }
    catch (e) { setError(String(e)); }
    finally { setRetrying(null); }
  };
  return <div className="periodic-review-history"><h3>Periodic review activity</h3>
    {error && <p className="error small" role="alert">{error}</p>}
    {!rows && !error && <p className="muted small">Loading review activity…</p>}
    {rows?.length === 0 && <p className="muted small">No periodic review has started. When enabled, Nitely tracks new merges from its starting point.</p>}
    {rows?.map((row) => <div key={row.repo} className="periodic-review-repo">
      <p className="small mono periodic-repo-name" title={row.repo}>{row.repo}</p>
      <p className="small">{row.pending_landings} merges awaiting review · {row.cursor ? <>Last reviewed <code>{row.cursor.slice(0, 8)}</code></> : <>Starting at <code>{row.baseline.slice(0, 8)}</code> · not reviewed yet</>}</p>
      {row.error && <p className="error small">{row.error}</p>}
      {row.jobs.map((job) => <details key={job.id} className="periodic-review-job">
        <summary><span>{job.state === "findings" ? "Findings" : job.state === "passed" ? "Reviewed" : job.state === "failed" ? "Review failed" : job.state === "running" ? "Reviewing" : job.state === "applying" ? "Updating cards" : job.state === "canceled" ? "Canceled" : "Queued"}</span><code>{job.base.slice(0, 8)} → {job.head.slice(0, 8)}</code><span className="muted small">{job.findings.length ? `${job.findings.length} finding${job.findings.length === 1 ? "" : "s"}` : job.reviewer ?? ""}</span></summary>
        {job.error && <p className="error small">{job.error}</p>}
        {job.state === "failed" && <button className="btn" disabled={isLive || retrying !== null} onClick={() => void retry(row.repo, job.id)}>{retrying === job.id ? "Retrying…" : "Retry review"}</button>}
        {job.findings.map((finding, i) => <p key={i} className="small periodic-review-finding"><strong>P{finding.priority}</strong> {finding.text} {finding.issue_key && <a href={`http://127.0.0.1:7423/i/${encodeURIComponent(finding.issue_key)}`}>{finding.issue_key}</a>} <span className="muted">({finding.action})</span></p>)}
      </details>)}
    </div>)}
  </div>;
}
