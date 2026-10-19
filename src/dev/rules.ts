// The browser preview's review and landing rules (src/Rules.tsx): one repo that commits its own
// playbook (read-only), one whose playbook Nitely keeps (editable, with rules that send changes to
// a person), and one with no playbook yet. Saves change the data until reload; the loosening check
// follows crates/nitely-core/src/rules.rs.
import type { Batching, DefaultsView, DryRun, LandBy, ReviewerPreview, ReviewerSaved, RuleDefaults, RuleField, RulesEdit, RulesView } from "../types";

const BUILT_IN_REVIEWER: Record<string, string> = { "*": "claude", claude: "codex", codex: "claude" };
const REVIEWERS = [
  { name: "claude", installed: true },
  { name: "codex", installed: true },
];

let defaults: RuleDefaults = { require_cross_vendor: true };

interface Stored {
  repo: string;
  project: string;
  root: string;
  source: RulesView["source"];
  path: string;
  integration: string;
  edit: RulesEdit;
}

const EMPTY: RulesEdit = { review_enabled: null, always_human: false, human_paths: [], blocked_paths: [], flags: [], reviewer: null, require_cross_vendor: null, land_by: null, focus: null };

const STORED: Stored[] = [
  {
    repo: "Nitely",
    project: "NIT",
    root: "C:\\Code\\Nitely",
    source: "repo",
    path: "C:\\Code\\Nitely\\.nitely\\playbook.toml",
    integration: "main",
    edit: { ...EMPTY, focus: "Only a human approves or answers asks. A change that breaks this is a P0 or P1 finding." },
  },
  {
    repo: "Orbit",
    project: "ORB",
    root: "C:\\Code\\Orbit",
    source: "stored",
    path: "C:\\Users\\you\\AppData\\Roaming\\nitely\\playbooks\\Orbit.toml",
    integration: "develop",
    edit: {
      ...EMPTY,
      human_paths: [
        { pattern: "^src/billing/", reason: "billing and money" },
        { pattern: "^migrations/", reason: "database migrations" },
      ],
      blocked_paths: [{ pattern: "^vendor/", reason: "updated by its own sync" }],
      flags: [{ name: "MONEY_OR_RELEASE", question: "the change alters pricing, payouts or the release process", reason: "" }],
      land_by: "auto",
    },
  },
  { repo: "Harbor", project: "HAR", root: "C:\\Code\\Harbor", source: "none", path: "C:\\Users\\you\\AppData\\Roaming\\nitely\\playbooks\\Harbor.toml", integration: "main", edit: { ...EMPTY } },
];

function field<T>(own: T | null, app: T | undefined, builtIn: T): RuleField<T> {
  const inherited = app ?? builtIn;
  const inherited_from = app === undefined ? "built-in" : "app";
  return { value: own ?? inherited, from: own === null ? inherited_from : "project", inherited, inherited_from };
}

function view(s: Stored): RulesView {
  const e = s.edit;
  return {
    repo: s.repo,
    root: s.root,
    source: s.source,
    path: s.path,
    editable: s.source !== "repo",
    reviewer_editable: true,
    review_enabled: field(e.review_enabled, undefined, true),
    read_only:
      s.source === "repo"
        ? `${s.repo} commits its own .nitely/playbook.toml. You can change review settings below. Other rules are set in that file; an agent can propose those changes for you to approve.`
        : null,
    integration: s.integration,
    always_human: e.always_human,
    human_paths: e.human_paths,
    blocked_paths: e.blocked_paths,
    flags: e.flags,
    reviewer: field(e.reviewer, defaults.reviewer, BUILT_IN_REVIEWER),
    require_cross_vendor: field(e.require_cross_vendor, defaults.require_cross_vendor, false),
    land_by: field<LandBy>(e.land_by, defaults.land_by, "author"),
    focus: field(e.focus, defaults.focus, ""),
    reviewers: REVIEWERS,
    batching: BATCHING[s.repo] ?? OFF,
  };
}

// How each sample repo batches (docs/batching.md): Nitely reviews small changes together and lands
// what waits together; Orbit measures batched review in shadow and asks for a queue it cannot run.
const OFF: Batching = {
  review: "off", review_window: "5m", review_max: 3, small_lines: 100, small_files: 5, max_lines: 1500,
  land: "off", land_window_seconds: 0, land_max: 5, land_alone: null,
};
const BATCHING: Record<string, Batching> = {
  Nitely: { ...OFF, review: "small", land: "waiting" },
  Orbit: { ...OFF, review: "shadow", land: "all", land_window_seconds: 300, land_alone: "a batch is gated in a build lane, and this repo has no [lanes]" },
};

/** The effective rules an edit makes, for the loosening check. */
function effective(e: RulesEdit, d: RuleDefaults) {
  return {
    enabled: e.review_enabled ?? true,
    always: e.always_human,
    human: e.human_paths,
    blocked: e.blocked_paths,
    flags: e.flags,
    cross: e.require_cross_vendor ?? d.require_cross_vendor ?? false,
    land: e.land_by ?? d.land_by ?? "author",
  };
}

function loosening(a: ReturnType<typeof effective>, b: ReturnType<typeof effective>): string[] {
  const out: string[] = [];
  if (a.always && !b.always) out.push("Every change no longer waits for a person.");
  for (const r of a.human.filter((r) => !b.human.some((n) => n.pattern === r.pattern))) out.push(`Changes to \`${r.pattern}\` (${r.reason}) no longer wait for a person.`);
  for (const r of a.blocked.filter((r) => !b.blocked.some((n) => n.pattern === r.pattern))) out.push(`Task branches may now change \`${r.pattern}\` (${r.reason}).`);
  for (const f of a.flags) {
    const n = b.flags.find((x) => x.name === f.name);
    if (!n) out.push(`The reviewer no longer answers ${f.name} (${f.question}).`);
    else if (n.question.trim() !== f.question.trim()) out.push(`${f.name} now asks "${n.question.trim()}" instead of "${f.question.trim()}".`);
  }
  if (a.cross && !b.cross) out.push("A review by the author's own vendor no longer waits for a person.");
  if (a.land === "human" && b.land !== "human") out.push("Changes no longer all wait for a person before they land.");
  return out;
}

function stored(repo: unknown): Stored {
  const s = STORED.find((x) => x.repo === String(repo));
  if (!s) throw `no repo ${String(repo)} in Nitely's config`;
  return s;
}

export function rules(project: string): RulesView[] {
  return STORED.filter((s) => s.project === project).map(view);
}

export function rulesCheck(args: Record<string, unknown>): string[] {
  const s = stored(args.repo);
  return loosening(effective(s.edit, defaults), effective(args.edit as RulesEdit, defaults));
}

export function rulesSave(args: Record<string, unknown>): RulesView {
  const s = stored(args.repo);
  if (s.source === "repo") throw `${s.repo} commits its own .nitely/playbook.toml; change it there`;
  const edit = args.edit as RulesEdit;
  const loose = loosening(effective(s.edit, defaults), effective(edit, defaults));
  if (loose.length && !args.confirm) throw `this loosens what sends changes to a person; confirm it first: ${loose.join(" ")}`;
  s.edit = structuredClone(edit);
  if (s.source === "none") s.source = "stored";
  return view(s);
}

export function reviewerPreview(args: Record<string, unknown>): ReviewerPreview {
  const s = stored(args.repo);
  const edit = args.edit as Pick<RulesEdit, "reviewer"> & { enabled?: boolean };
  const line = (map: Record<string, string> | null) => map ? `reviewer = { ${Object.entries(map).map(([k, v]) => `${JSON.stringify(k)} = ${JSON.stringify(v)}`).join(", ")} }\n` : "";
  const before = (s.edit.review_enabled === null ? "" : `enabled = ${s.edit.review_enabled}\n`) + line(s.edit.reviewer);
  const enabled = edit.enabled ?? s.edit.review_enabled;
  const after = (enabled === null ? "" : `enabled = ${enabled}\n`) + line(edit.reviewer);
  return { before, after, diff: `${before ? `- ${before}` : ""}${after ? `+ ${after}` : ""}`, destination: `Commit to ${s.path} on ${s.integration}; pushed with the next land.` };
}

export function reviewerSave(args: Record<string, unknown>): ReviewerSaved {
  const s = stored(args.repo);
  const expected = args.preview as ReviewerPreview;
  const current = reviewerPreview(args);
  if (expected.before !== current.before || expected.after !== current.after) throw "The playbook changed; preview again.";
  s.edit.reviewer = structuredClone((args.edit as Pick<RulesEdit, "reviewer">).reviewer);
  const enabled = (args.edit as { enabled?: boolean }).enabled;
  if (enabled !== undefined) s.edit.review_enabled = enabled;
  return { view: view(s), done: `Committed review settings on ${s.integration}; pushed with the next land. New review launches use these settings.` };
}

export function rulesBranches(args: Record<string, unknown>): string[] {
  const s = stored(args.repo);
  const key = s.project.toLowerCase();
  return [`claude/${key}-31-pulse-home`, `codex/${key}-30-review-rounds`, `claude/${key}-44-tray-count`, "spike/try-sqlite-fts"];
}

export function rulesDryRun(args: Record<string, unknown>): DryRun {
  const s = stored(args.repo);
  const branch = String(args.branch);
  const e = effective(s.edit, defaults);
  const files = branch.includes("44") ? ["src/billing/invoice.ts", "src/tray.ts"] : ["src/Pulse.tsx", "src/pulse.css", "src/types.ts"];
  const vendor = branch.split("/")[0];
  const reviewer = vendor === "codex" ? "claude" : "codex";
  const reasons = files.flatMap((f) => e.human.filter((r) => new RegExp(r.pattern).test(f)).map((r) => `${r.reason}: ${f}`));
  if (e.always) reasons.unshift("this playbook sends every change to a human");
  if (e.land === "human") reasons.push("this playbook lands only what a human approved");
  if (e.enabled && vendor !== "claude" && vendor !== "codex" && e.cross) reasons.push(`${reviewer} review (author's vendor unknown), and this playbook requires a review by another vendor`);
  if (!e.enabled) {
    reasons.push(...e.flags.map((f) => `Unevaluated reviewer condition ${f.name}: ${f.question} (automated review is disabled)`));
    if (e.cross) reasons.push("Unevaluated cross-vendor review requirement (automated review is disabled)");
  }
  const outcome = reasons.length
    ? e.land === "author"
      ? "Once the review passes it waits for a person in Needs You; after they approve, the agent lands it."
      : "Once the review passes it waits for a person in Needs You; Nitely lands it as soon as they approve."
    : e.land === "author"
      ? "Once the review passes it moves to In Review, and the agent lands it."
      : "Once the review passes Nitely lands it in the same submit.";
  return {
    branch,
    base: s.integration,
    files,
    reviewer: e.enabled ? reviewer : null,
    reviewer_note: !e.enabled ? "Review disabled for this project" : vendor === "claude" || vendor === "codex" ? "" : "author's vendor unknown",
    reviewer_error: null,
    reasons,
    flags: e.flags.map((f) => `${f.name}: ${f.question}`),
    blocked: null,
    outcome: e.enabled ? outcome : `Review disabled for this project. ${outcome.replaceAll("the review passes", "the gate passes")}`,
  };
}

export function ruleDefaults(): DefaultsView {
  return { defaults, built_in_reviewer: BUILT_IN_REVIEWER, reviewers: REVIEWERS };
}

export function ruleDefaultsCheck(args: Record<string, unknown>): string[] {
  return loosening(effective(EMPTY, defaults), effective(EMPTY, args.defaults as RuleDefaults));
}

export function ruleDefaultsSave(args: Record<string, unknown>): DefaultsView {
  const next = args.defaults as RuleDefaults;
  const loose = loosening(effective(EMPTY, defaults), effective(EMPTY, next));
  if (loose.length && !args.confirm) throw `this loosens what sends changes to a person; confirm it first: ${loose.join(" ")}`;
  defaults = structuredClone(next);
  return ruleDefaults();
}
