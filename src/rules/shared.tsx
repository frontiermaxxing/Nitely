import type { LandBy, RuleField, RulesEdit, RulesView } from "../types";
import { VENDOR_LABEL, vendorOf, type ValueSource } from "../ui";

/** Who lands a change, in the words the playbook docs use. */
export const LAND_BY: Record<LandBy, { label: string; why: string }> = {
  author: { label: "The agent that wrote it", why: "A passing review moves it to In Review, and the agent calls land (after you approve, when you must)." },
  auto: { label: "Nitely, as soon as it can", why: "Nitely lands it in the same submit when the review passes and nothing needs you, and as soon as you approve it otherwise." },
  human: { label: "Nitely, after you approve", why: "Every change waits for you in Needs You, and Nitely lands it as soon as you approve." },
};

const FROM: Record<RuleField<unknown>["from"], string> = { project: "this project", app: "app default", "built-in": "Nitely's default" };

/** Where a value comes from, beside the value. */
export function From(props: { from: RuleField<unknown>["from"] }) {
  return <span className={`rules-from rules-from-${props.from}`}>{FROM[props.from]}</span>;
}

/** The source tag for where a rule's value comes from. */
export function sourceOf(from: RuleField<unknown>["from"]): ValueSource {
  return from === "built-in" ? "nitely" : from;
}

/** "Codex" for "codex", the reviewer name as the playbook has it otherwise. */
export function reviewerName(name: string): string {
  const v = vendorOf(name);
  return v === "other" ? name : VENDOR_LABEL[v];
}

/** The edit a view starts from: what the project sets itself, nothing it inherits. */
export function draftOf(v: RulesView): RulesEdit {
  const own = <T,>(f: RuleField<T>) => (f.from === "project" ? f.value : null);
  return {
    review_enabled: own(v.review_enabled),
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
export function waitsSummary(d: RulesEdit, landBy: LandBy, crossVendor: boolean): string {
  if (d.review_enabled === false && (d.flags.length || crossVendor)) return "Reviewer conditions are unevaluated while review is off, so changes wait for your decision.";
  if (d.always_human || landBy === "human") return "Every change waits for you before it lands.";
  const parts: string[] = [];
  const reasons = [...new Set(d.human_paths.filter((r) => r.pattern.trim()).map((r) => r.reason.trim() || r.pattern.trim()))];
  if (reasons.length) parts.push(`it touches ${reasons.join(", ")}`);
  if (d.flags.length) parts.push(`the reviewer answers yes to ${d.flags.map((f) => f.name || "a question").join(" or ")}`);
  if (crossVendor) parts.push("the reviewer is the author's own vendor");
  return parts.length ? `A change waits for you when ${parts.join(", or when ")}. Anything else can land after its required checks.` : "Nothing waits for you: a change can land after its required checks.";
}

/** The draft every section of one repo's rules edits, and how a section changes it. */
export interface RulesDraft {
  view: RulesView;
  draft: RulesEdit;
  /** Not editable: the playbook is read-only here. */
  ro: boolean;
  busy: boolean;
  landBy: LandBy;
  crossVendor: boolean;
  patch: (p: Partial<RulesEdit>) => void;
}
