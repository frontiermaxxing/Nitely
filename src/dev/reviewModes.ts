import type { ModeOptions, ModePreset, PeriodicStatus, ProjectReviewMode, ReviewMode } from "../reviewModes";
import { INHERITED_OPTIONS, optionsFor } from "../reviewModes";

const inherited: ModeOptions = { test_depth: null, pre_review: null, land_batch: null, shared_lanes: null, full_backstop: null, audit_percent: null, periodic: null };
const periodic = { enabled: true, every_landings: 5, max_delay_seconds: 1800, reviewer: "configured", findings_strategy: "reopen_or_create" };
const backstop = { enabled: true, landings: 5, delay_seconds: 1800 };
const presets: ModePreset[] = [
  { mode: "yolo", options: { test_depth: "off", pre_review: "off", land_batch: true, shared_lanes: true, full_backstop: { ...backstop, enabled: false }, audit_percent: 0, periodic: { ...periodic, enabled: false } } },
  { mode: "fast", options: { test_depth: "basic", pre_review: "off", land_batch: true, shared_lanes: true, full_backstop: { ...backstop, enabled: false }, audit_percent: 0, periodic } },
  { mode: "balanced", options: { test_depth: "risk", pre_review: "batched", land_batch: true, shared_lanes: true, full_backstop: backstop, audit_percent: 10, periodic } },
  { mode: "thorough", options: { test_depth: "full", pre_review: "individual", land_batch: false, shared_lanes: true, full_backstop: { ...backstop, enabled: false }, audit_percent: 0, periodic: { ...periodic, enabled: false } } },
];
const values = new Map<string, ProjectReviewMode>();
const retried = new Set<string>();
export function reviewMode(project: string): ProjectReviewMode {
  return structuredClone(values.get(project) ?? { project, mode: "custom", revision: 0, overrides: inherited, presets });
}
export function saveReviewMode(project: string, mode: ReviewMode, overrides: ModeOptions, revision: number) {
  const before = reviewMode(project);
  if (revision !== before.revision) throw new Error("Review settings changed; reload them before saving.");
  if (mode !== "custom" && JSON.stringify(overrides) !== JSON.stringify(INHERITED_OPTIONS)) throw new Error("Preset controls are fixed; select Custom to override them.");
  const saved = { ...before, mode, revision: revision + 1, overrides: structuredClone(overrides) };
  values.set(project, structuredClone(saved));
  return saved;
}
export function effectiveModes(project: string) {
  const mode = reviewMode(project);
  const options = optionsFor(mode.mode, mode.presets, mode.overrides);
  const fallback = { ...presets.find((p) => p.mode === "thorough")!.options, land_batch: true };
  const effective = Object.fromEntries(Object.entries(options).map(([key, value]) => [key, value ?? fallback[key as keyof ModeOptions]])) as unknown as ModeOptions;
  return [{ repo: "Sample project", options: effective, warnings: [] }];
}
export function periodicReviews(project: string): PeriodicStatus[] {
  if (project !== "NIT") return [];
  return [{
    project, repo: "C:/Code/sample-project/.git", cursor: "8ac31f0d2770000000000000000000000000000000",
    baseline: "4fa182b10000000000000000000000000000000000", observed_head: "920f13ba0000000000000000000000000000000000",
    pending_landings: 2, pending_since: Math.floor(Date.now() / 1000) - 300, last_success_at: Math.floor(Date.now() / 1000) - 900, error: null,
    jobs: [
      { id: 2, base: "8ac31f0d2770000000000000000000000000000000", head: "920f13ba0000000000000000000000000000000000", state: retried.has(project) ? "pending" : "failed", attempt: retried.has(project) ? 0 : 1, created_at: Math.floor(Date.now() / 1000) - 120, completed_at: retried.has(project) ? null : Math.floor(Date.now() / 1000) - 60, reviewer: "codex", error: retried.has(project) ? null : "Reviewer was unavailable. The reviewed position has not advanced; a retry is pending.", findings: [] },
      { id: 1, base: "4fa182b10000000000000000000000000000000000", head: "8ac31f0d2770000000000000000000000000000000", state: "findings", attempt: 1, created_at: Math.floor(Date.now() / 1000) - 1200, completed_at: Math.floor(Date.now() / 1000) - 900, reviewer: "codex", error: null, findings: [{ priority: 2, text: "A pending request can lose its retry after an interrupted restart.", issue_key: "NIT-18", action: "filed" }] },
    ],
  }];
}
export function retryPeriodic(project: string, repo: string, id: number) {
  if (!periodicReviews(project).some((r) => r.repo === repo && r.jobs.some((j) => j.id === id && j.state === "failed"))) throw new Error("Failed review not found");
  retried.add(project);
}
