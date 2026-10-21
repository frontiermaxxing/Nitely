import { useEffect, useRef, useState } from "react";
import { invoke, isLive } from "./backend";
import { MODE_DESCRIPTION, MODE_LABEL, editMode, modeSave, modeValidation, optionsFor, type EffectiveRepository, type ModeDraft, type ModeOptions, type PeriodicStatus, type ProjectReviewMode, type ReviewMode } from "./reviewModes";
import { Button, RadioCards, Segmented, Select, SettingsRow, SettingsSection, TextField, type ChoiceOption, type SelectOption } from "./ui";
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

/** A setting a mode can leave to each repository: its choices, plus "Repository settings" (null). */
function Choice<T extends string>(props: { label: string; value: T | null; choices: [T, string][]; disabled: boolean; onChange: (value: T | null) => void }) {
  const options: SelectOption<T | "repo">[] = [
    { value: "repo", label: "Repository settings", detail: "Each repository keeps its own rules" },
    ...props.choices.map(([value, label]) => ({ value, label })),
  ];
  return <Select<T | "repo"> aria-label={props.label} value={props.value ?? "repo"} options={options} disabled={props.disabled} onChange={(v) => props.onChange(v === "repo" || v === null ? null : v)} />;
}
/** On, off, or left to each repository. */
function Toggle(props: { label: string; value: boolean | null; disabled: boolean; onChange: (value: boolean | null) => void }) {
  const options: ChoiceOption<"repo" | "on" | "off">[] = [{ value: "repo", label: "Repository" }, { value: "on", label: "On" }, { value: "off", label: "Off" }];
  return <Segmented aria-label={props.label} options={options} value={props.value === null ? "repo" : props.value ? "on" : "off"} disabled={props.disabled} onChange={(v) => props.onChange(v === "repo" ? null : v === "on")} />;
}
function Count({ id, value, min = 1, max, disabled, onChange }: { id: string; value: number; min?: number; max?: number; disabled: boolean; onChange: (n: number) => void }) {
  return <TextField id={id} className="review-mode-count" type="number" min={min} max={max} step="1" disabled={disabled} value={Number.isFinite(value) ? value : ""} onChange={(e) => onChange(e.target.value === "" ? NaN : Number(e.target.value))} />;
}
const PERIODIC_REVIEWERS: SelectOption[] = [{ value: "configured", label: "Repository reviewer routing" }, { value: "codex", label: "Codex" }, { value: "claude", label: "Claude" }, { value: "cursor", label: "Cursor" }];
const FINDINGS: SelectOption[] = [{ value: "reopen_or_create", label: "Reopen the matching card or file a follow-up" }, { value: "create", label: "File linked follow-up cards" }];

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
  const off = control.busy || isLive;
  const audits: [string, string][] = [["0", "Off"], ["10", "10%"], ["25", "25%"], ["50", "50%"], ["100", "Every eligible change"]];
  if (options && options.audit_percent !== null && !audits.some(([v]) => Number(v) === options.audit_percent)) audits.push([String(options.audit_percent), `${options.audit_percent}%`]);
  return <div className="review-mode-settings">
    <SettingsSection title="Review mode" description={<>How this project checks changes before and after they reach main, in every connected repository.{saved && <> Saved: <strong>{MODE_LABEL[saved.mode]}</strong>.</>}</>}>
      {!saved && !control.error && <p className="muted small review-mode-loading">Loading review modes…</p>}
      {saved && draft && <SettingsRow label="Mode" why="The settings below show what it does. Editing one of them makes it Custom.">
        <RadioCards<ReviewMode> aria-label="Review mode" options={(Object.keys(MODE_LABEL) as ReviewMode[]).map((mode) => ({ value: mode, label: MODE_LABEL[mode], why: MODE_DESCRIPTION[mode] }))} value={draft.mode} disabled={off} onChange={pick} />
      </SettingsRow>}
    </SettingsSection>
    {saved && draft && options && <>
      <SettingsSection title="Before merging" description="Repository settings keeps each repository's own rules for that check.">
        <SettingsRow label="Automatic tests" why="Basic and risk-based checks use the repository's configured commands. Without that configuration the full checks run.">
          <Choice label="Automatic tests" value={options.test_depth} choices={[["off", "Off"], ["basic", "Basic checks"], ["risk", "Match the change's risk"], ["full", "Full suite"]]} disabled={off} onChange={(test_depth) => edit({ test_depth })} />
        </SettingsRow>
        <SettingsRow label="PR review">
          <Choice label="PR review" value={options.pre_review} choices={[["off", "Off"], ["individual", "Review each change"], ["batched", "Review changes in batches"]]} disabled={off} onChange={(pre_review) => edit({ pre_review })} />
        </SettingsRow>
        <SettingsRow label="Batch merges" why={options.land_batch && options.test_depth !== "off" ? "When changes land together, the combined result runs the full checks." : undefined}>
          <Toggle label="Batch merges" value={options.land_batch} disabled={off} onChange={(land_batch) => edit({ land_batch })} />
        </SettingsRow>
        <SettingsRow label="Shared build lanes">
          <Toggle label="Shared build lanes" value={options.shared_lanes} disabled={off} onChange={(shared_lanes) => edit({ shared_lanes })} />
        </SettingsRow>
      </SettingsSection>
      <SettingsSection title="After merging" description="Periodic reviews check what has landed without holding up merges. A failed review keeps its place, so no change is skipped.">
        <SettingsRow label="Periodic review of main">
          <Toggle label="Periodic review of main" value={options.periodic === null ? null : options.periodic.enabled} disabled={off} onChange={(v) => edit({ periodic: v === null ? null : { ...periodic, enabled: v } })} />
        </SettingsRow>
        {options.periodic?.enabled && <>
          <SettingsRow label="Review after this many merges" htmlFor="review-mode-every">
            <Count id="review-mode-every" value={periodic.every_landings} max={1000} disabled={off} onChange={(every_landings) => edit({ periodic: { ...periodic, every_landings } })} />
          </SettingsRow>
          <SettingsRow label="Or after this many minutes" why="0 is no time limit." htmlFor="review-mode-minutes">
            <Count id="review-mode-minutes" value={periodic.max_delay_seconds / 60} min={0} max={10080} disabled={off} onChange={(n) => edit({ periodic: { ...periodic, max_delay_seconds: n * 60 } })} />
          </SettingsRow>
          <SettingsRow label="Periodic reviewer">
            <Select aria-label="Periodic reviewer" value={periodic.reviewer} options={PERIODIC_REVIEWERS} disabled={off} onChange={(reviewer) => reviewer && edit({ periodic: { ...periodic, reviewer } })} />
          </SettingsRow>
          <SettingsRow label="When a review finds a problem">
            <Select aria-label="When a review finds a problem" value={periodic.findings_strategy} options={FINDINGS} disabled={off} onChange={(findings_strategy) => findings_strategy && edit({ periodic: { ...periodic, findings_strategy } })} />
          </SettingsRow>
        </>}
        <SettingsRow label="Full-suite checks on main">
          <Toggle label="Full-suite checks on main" value={options.full_backstop === null ? null : options.full_backstop.enabled} disabled={off} onChange={(v) => edit({ full_backstop: v === null ? null : { ...backstop, enabled: v } })} />
        </SettingsRow>
        {options.full_backstop?.enabled && <>
          <SettingsRow label="Full suite after this many light or unchecked merges" htmlFor="review-mode-landings">
            <Count id="review-mode-landings" value={backstop.landings} max={1000} disabled={off} onChange={(landings) => edit({ full_backstop: { ...backstop, landings } })} />
          </SettingsRow>
          <SettingsRow label="Full suite after this many minutes" htmlFor="review-mode-backstop-minutes">
            <Count id="review-mode-backstop-minutes" value={backstop.delay_seconds / 60} max={1440} disabled={off} onChange={(n) => edit({ full_backstop: { ...backstop, delay_seconds: n * 60 } })} />
          </SettingsRow>
        </>}
        <SettingsRow label="Sampled audits of light changes">
          <Choice label="Sampled audits of light changes" value={options.audit_percent === null ? null : String(options.audit_percent)} choices={audits} disabled={off} onChange={(v) => edit({ audit_percent: v === null ? null : Number(v) })} />
        </SettingsRow>
      </SettingsSection>
      <div className="review-mode-save">
        <Button variant="primary" disabled={!changed || !!validation || off} onClick={() => void control.save(draft)}>{control.busy ? "Saving…" : "Save review mode"}</Button>
        <span className="muted small">{changed ? "Unsaved changes" : "Saved"}</span>
      </div>
      {validation && <p className="error small" role="alert">{validation}</p>}
      <p className="settings-hint">Manual checks, release requirements and required human approvals stay separate. This controls Nitely's automatic work; hosted CI has its own workflow settings.</p>
      {isLive && <p className="settings-hint">Live preview is read-only. Change review modes in the desktop app.</p>}
      <EffectiveModes project={saved.project} revision={saved.revision} />
      <PeriodicReviewHistory project={saved.project} version={version} />
    </>}
    {(control.error || localError) && <p className="error small" role="alert">{control.error || localError}</p>}
  </div>;
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
        {job.state === "failed" && <Button disabled={isLive || retrying !== null} onClick={() => void retry(row.repo, job.id)}>{retrying === job.id ? "Retrying…" : "Retry review"}</Button>}
        {job.findings.map((finding, i) => <p key={i} className="small periodic-review-finding"><strong>P{finding.priority}</strong> {finding.text} {finding.issue_key && <a href={`http://127.0.0.1:7423/i/${encodeURIComponent(finding.issue_key)}`}>{finding.issue_key}</a>} <span className="muted">({finding.action})</span></p>)}
      </details>)}
    </div>)}
  </div>;
}
