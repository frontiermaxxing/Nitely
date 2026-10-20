export type ReviewMode = "yolo" | "fast" | "balanced" | "thorough" | "custom";
export interface BackstopOptions { enabled: boolean; landings: number; delay_seconds: number }
export interface PeriodicOptions {
  enabled: boolean;
  every_landings: number;
  max_delay_seconds: number;
  reviewer: string;
  findings_strategy: string;
}
export interface ModeOptions {
  test_depth: "off" | "basic" | "risk" | "full" | null;
  pre_review: "off" | "individual" | "batched" | null;
  land_batch: boolean | null;
  shared_lanes: boolean | null;
  full_backstop: BackstopOptions | null;
  audit_percent: number | null;
  periodic: PeriodicOptions | null;
}
export interface ModePreset { mode: ReviewMode; options: ModeOptions }
export interface ProjectReviewMode {
  project: string;
  mode: ReviewMode;
  revision: number;
  overrides: ModeOptions;
  presets: ModePreset[];
}
export const INHERITED_OPTIONS: ModeOptions = { test_depth: null, pre_review: null, land_batch: null, shared_lanes: null, full_backstop: null, audit_percent: null, periodic: null };
export interface ModeDraft { mode: ReviewMode; overrides: ModeOptions }
export const MODE_LABEL: Record<ReviewMode, string> = { yolo: "YOLO", fast: "Fast", balanced: "Balanced", thorough: "Thorough", custom: "Custom" };
export const MODE_DESCRIPTION: Record<ReviewMode, string> = {
  yolo: "No automatic tests or reviews.",
  fast: "Basic checks. Review main periodically.",
  balanced: "Risk-based checks and batched reviews.",
  thorough: "Full checks and review before each merge.",
  custom: "Choose every setting individually.",
};
export function optionsFor(mode: ReviewMode, presets: ModePreset[], custom: ModeOptions): ModeOptions {
  if (mode === "custom") return structuredClone(custom);
  const preset = presets.find((p) => p.mode === mode);
  if (!preset) throw new Error(`The ${MODE_LABEL[mode]} preset is unavailable. Reload project settings.`);
  return structuredClone(preset.options);
}
export function editMode(draft: ModeDraft, edit: Partial<ModeOptions>): ModeDraft {
  return { mode: "custom", overrides: { ...structuredClone(draft.overrides), ...structuredClone(edit) } };
}
export function modeSave(draft: ModeDraft): ModeDraft {
  // Preset policy belongs to the backend; only Custom submits overrides.
  return { mode: draft.mode, overrides: structuredClone(draft.mode === "custom" ? draft.overrides : INHERITED_OPTIONS) };
}
export function modeValidation(options: ModeOptions): string | null {
  const positive = (n: number) => Number.isSafeInteger(n) && n > 0;
  const p = options.periodic;
  if (p && (!positive(p.every_landings) || p.every_landings > 1000 || !Number.isSafeInteger(p.max_delay_seconds) || p.max_delay_seconds < 0 || p.max_delay_seconds > 604800))
    return "Periodic review needs at least one merge, up to 1,000. Its time limit must be between zero (no time limit) and 10,080 minutes.";
  const b = options.full_backstop;
  if (b && (!positive(b.landings) || b.landings > 1000 || !positive(b.delay_seconds) || b.delay_seconds > 86400))
    return "Full-suite checks need 1–1,000 merges and a time interval greater than zero, up to 1,440 minutes.";
  if (options.audit_percent !== null && (!Number.isSafeInteger(options.audit_percent) || (options.audit_percent !== 0 && (options.audit_percent < 10 || options.audit_percent > 100))))
    return "Audit sampling must be off (0%) or between 10% and 100%.";
  return null;
}
export interface PeriodicFinding { priority: number; text: string; issue_key: string | null; action: string }
export interface PeriodicJob {
  id: number; base: string; head: string; state: string; attempt: number;
  created_at: number; completed_at: number | null; reviewer: string | null;
  error: string | null; findings: PeriodicFinding[];
}
export interface PeriodicStatus {
  repo: string; project: string; cursor: string | null; baseline: string;
  observed_head: string; pending_landings: number; pending_since: number | null;
  last_success_at: number | null; error: string | null; jobs: PeriodicJob[];
}
export interface EffectiveRepository { repo: string; options: ModeOptions; warnings: string[] }
