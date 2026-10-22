import type { ModeOptions, ModePreset, PeriodicStatus, ProjectReviewMode, ReviewMode } from "../reviewModes";
import { INHERITED_OPTIONS, optionsFor } from "../reviewModes";
import type { PathDryRun, Pipeline, PipelineFact, PipelineSource } from "../rules/pipeline";

// Mirrors project_modes::preset in nitely-core.
const periodic = { enabled: false, every_landings: 5, max_delay_seconds: 1800, reviewer: "configured", findings_strategy: "reopen_or_create" };
const backstop = { enabled: true, landings: 5, delay_seconds: 1800 };
const presets: ModePreset[] = [
  { mode: "fast", options: { test_depth: "risk", pre_review: "batched", land_batch: true, shared_lanes: true, full_backstop: backstop, audit_percent: 10, periodic, goal_branches: true, item_review: "off", checkpoint: { items: 10, daily: true }, goal_plans: "asked", stalled_reviews: "fix" } },
  { mode: "careful", options: { test_depth: "risk", pre_review: "individual", land_batch: true, shared_lanes: true, full_backstop: backstop, audit_percent: 10, periodic, goal_branches: true, item_review: "advisory", checkpoint: { items: 5, daily: true }, goal_plans: "asked", stalled_reviews: "fix" } },
  { mode: "release", options: { test_depth: "full", pre_review: "individual", land_batch: false, shared_lanes: true, full_backstop: { ...backstop, landings: 1 }, audit_percent: 0, periodic, goal_branches: false, item_review: "blocking", checkpoint: null, goal_plans: "approve", stalled_reviews: "ask" } },
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
  // Mirrors project_modes::builtin: what Custom leaves unset is Nitely's default, not the playbook's.
  const fallback: ModeOptions = { ...presets.find((p) => p.mode === "release")!.options, full_backstop: { enabled: false, landings: 5, delay_seconds: 1800 }, audit_percent: 10, goal_branches: false, item_review: "off", checkpoint: null, goal_plans: "approve", stalled_reviews: "ask" };
  const effective = Object.fromEntries(Object.entries(options).map(([key, value]) => [key, value ?? fallback[key as keyof ModeOptions]])) as unknown as ModeOptions;
  return [{ repo: "Sample project", options: effective, warnings: [], backstop_off: effective.full_backstop?.enabled === false }];
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

/** Mirrors lands_pipeline::pipeline for the sample data: each value from the mode, else the sample playbook. */
export function landsPipeline(project: string, repos: string[]): Pipeline[] {
  const mode = reviewMode(project);
  const chosen = optionsFor(mode.mode, mode.presets, mode.overrides);
  const src = (set: boolean, _inPlaybook?: boolean): PipelineSource =>
    mode.mode !== "custom" ? { kind: "preset", mode: mode.mode } : set ? { kind: "custom" } : { kind: "builtin" };
  const depth = chosen.test_depth ?? "full";
  const audit = chosen.audit_percent ?? 10;
  const audited = audit > 0 ? `; ${audit}% audited` : "";
  const checks: PipelineFact =
    depth === "off" ? { value: "Skipped", why: "No automatic tests run before landing", tone: "off", source: src(true, false) }
    : depth === "full" ? { value: "Full gate", why: "Every command in the playbook's gate", tone: "agent", source: src(chosen.test_depth !== null, true) }
    : { value: "Quick checks", why: `Compile, lint and the affected tests; protected or unmapped changes get the full gate${audited}`, tone: "agent", source: src(chosen.test_depth !== null, true) };
  const pre = chosen.pre_review ?? "individual";
  const review: PipelineFact =
    pre === "off" ? { value: "Off", why: "Changes with no goal land unreviewed", tone: "off", source: src(true, false) }
    : pre === "batched" ? { value: "One per batch", why: "One blocking review per batch of changes with no goal", tone: "agent", source: src(chosen.pre_review !== null, false) }
    : { value: "Each change", why: "A blocking review for each change with no goal", tone: "agent", source: src(chosen.pre_review !== null, false) };
  const item = { off: "Items land on quick checks, unreviewed", advisory: "Each item is reviewed without blocking", blocking: "Each item gets a blocking review" }[chosen.item_review ?? "off"];
  const cp = chosen.checkpoint;
  const plans = { approve: "Each plan waits for your approval", asked: "Plans you asked for start by themselves", all: "Every plan starts by itself" }[chosen.goal_plans ?? "approve"];
  // As goal_branch::goal_items_route: a goal branch lands items unreviewed, so a mode that reviews
  // each item keeps them on main.
  const throughBranch = chosen.goal_branches && (chosen.item_review ?? "off") === "off";
  const goals: PipelineFact = throughBranch
    ? { value: "Through a goal branch", why: `${item}; main gets the goal at each checkpoint (a coherent slice${cp ? `, ${cp.items} items${cp.daily ? " or a day" : ""}` : ""}): one full run, one review. ${plans}`, tone: "agent", source: src(chosen.goal_branches !== null || chosen.goal_plans !== null, false) }
    : { value: "Straight to main", why: `${chosen.goal_branches ? "The mode reviews each goal item, and a goal branch lands items unreviewed, so they land on main" : "Each goal item lands on main like any other change"}. ${plans}`, tone: "agent", source: src(chosen.goal_branches !== null || chosen.goal_plans !== null, false) };
  const lands: PipelineFact = chosen.land_batch ?? false
    ? { value: "Batches of up to 5", why: "What waits lands on one full run, bisected if it fails", tone: "agent", source: src(chosen.land_batch !== null, true) }
    : { value: "Each change alone", why: "One gate and one push per change", tone: "agent", source: src(true, true) };
  const b = chosen.full_backstop ?? { enabled: false, landings: 5, delay_seconds: 1800 };
  const after: PipelineFact = b?.enabled
    ? { value: "Full suite on main", why: `Runs ${b.landings === 1 ? "after every land" : `every ${b.landings} lands or ${Math.ceil(b.delay_seconds / 60)} min`}; a failure is reverted`, tone: "agent", source: src(true, false) }
    : { value: "Nothing", why: "The full suite on main is off", tone: "off", source: src(chosen.full_backstop !== null) };
  const suites: PipelineFact = { value: "In CI", why: "cargo test --workspace on GitHub Actions, the rest in a build lane; local when CI is down", tone: "agent", source: { kind: "playbook" } };
  const warnings = b?.enabled ? [] : ["The full suite on main is off: nothing checks main after reduced checks land."];
  return repos.map((repo) => ({ repo, mode: mode.mode, checks, review, goals, lands, suites, after, warnings }));
}

/** Mirrors lands_pipeline::dry_run on the sample map: src/ is the web component, Cargo files keep the full gate. */
export function landsDryRun(project: string, path: string): PathDryRun {
  const mode = reviewMode(project);
  const depth = optionsFor(mode.mode, mode.presets, mode.overrides).test_depth ?? "full";
  const p = path.trim().split(String.fromCharCode(92)).join("/");
  const full = ["npm test", "npm run build", "cargo test --workspace", "cargo clippy --workspace -- -D warnings"];
  if (p.startsWith("vendor/")) return { path: p, checks: "refused", commands: [], why: "the branch changes " + p + ", which task branches may not change: third-party code", waits: [] };
  const waits = p.startsWith("crates/nitely-core/migrations/") ? ["a schema change: " + p] : [];
  if (depth === "off") return { path: p, checks: "none", commands: [], why: "Automatic tests are off", waits };
  if (depth === "full") return { path: p, checks: "full", commands: full, why: "The mode runs the full gate on every change", waits };
  if (waits.length) return { path: p, checks: "full", commands: full, why: "A path that needs you keeps the full gate", waits };
  if (/^src\//.test(p)) return { path: p, checks: "quick", commands: ["node node_modules/typescript/bin/tsc --noEmit", "npm run build", "npm test"], why: "Owned by web, from the test map; a larger change can run more", waits };
  if (/^crates\/nitely-cli\//.test(p)) return { path: p, checks: "quick", commands: ["node node_modules/typescript/bin/tsc --noEmit", "cargo test -p nitely-cli"], why: "Owned by cli, from the test map; a larger change can run more", waits };
  return { path: p, checks: "full", commands: full, why: /(^|\/)(Cargo\.(toml|lock)|package(-lock)?\.json)$/.test(p) ? "A build, lock or policy file keeps the full gate" : "No component in the test map owns it", waits };
}
