import type { ModeOptions, ModePreset, PeriodicStatus, ProjectReviewMode, ReviewMode } from "../reviewModes";
import { INHERITED_OPTIONS, optionsFor } from "../reviewModes";

// Mirrors project_modes::preset in nitely-core.
const periodic = { enabled: false, every_landings: 5, max_delay_seconds: 1800, reviewer: "configured", findings_strategy: "reopen_or_create" };
const backstop = { enabled: true, landings: 5, delay_seconds: 1800 };
const presets: ModePreset[] = [
  { mode: "fast", options: { test_depth: "risk", pre_review: "batched", land_batch: true, shared_lanes: true, full_backstop: backstop, audit_percent: 10, periodic, goal_branches: true, item_review: "off", checkpoint: { items: 10, daily: true } } },
  { mode: "careful", options: { test_depth: "risk", pre_review: "individual", land_batch: true, shared_lanes: true, full_backstop: backstop, audit_percent: 10, periodic, goal_branches: true, item_review: "advisory", checkpoint: { items: 5, daily: true } } },
  { mode: "release", options: { test_depth: "full", pre_review: "individual", land_batch: false, shared_lanes: true, full_backstop: { ...backstop, landings: 1 }, audit_percent: 0, periodic, goal_branches: false, item_review: "blocking", checkpoint: null } },
];
// The sample project shows the notice a project moved from a retired mode sees until it saves.
const migrated: Record<string, string[]> = {
  NIT: ["Balanced is now Careful.", "Changes with no goal are reviewed one by one instead of in batches.", "Goals land through a goal branch: each item is reviewed without blocking, and each checkpoint gets one blocking review.", "Periodic review of main is off: the checkpoint review covers it."],
};
const values = new Map<string, ProjectReviewMode>();
const retried = new Set<string>();
export function reviewMode(project: string): ProjectReviewMode {
  return structuredClone(values.get(project) ?? (migrated[project]
    ? { project, mode: "careful", revision: 2, overrides: INHERITED_OPTIONS, presets, explicit: true, migrated_from: "balanced", migration_changes: migrated[project] }
    : { project, mode: "custom", revision: 0, overrides: INHERITED_OPTIONS, presets, explicit: false, migrated_from: null, migration_changes: [] }));
}
export function saveReviewMode(project: string, mode: ReviewMode, overrides: ModeOptions, revision: number) {
  const before = reviewMode(project);
  if (revision !== before.revision) throw new Error("Review settings changed; reload them before saving.");
  if (mode !== "custom" && JSON.stringify(overrides) !== JSON.stringify(INHERITED_OPTIONS)) throw new Error("Preset controls are fixed; select Custom to override them.");
  const saved = { ...before, mode, revision: revision + 1, overrides: structuredClone(overrides), explicit: true, migrated_from: null, migration_changes: [] };
  values.set(project, structuredClone(saved));
  return saved;
}
export function effectiveModes(project: string) {
  const mode = reviewMode(project);
  const options = optionsFor(mode.mode, mode.presets, mode.overrides);
  const fallback = { ...presets.find((p) => p.mode === "release")!.options, land_batch: true };
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
