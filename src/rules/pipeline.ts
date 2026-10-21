import { MODE_LABEL, type ReviewMode } from "../reviewModes";

/** Where a value comes from (lands_pipeline.rs `Source`). */
export type PipelineSource = { kind: "preset"; mode: ReviewMode } | { kind: "custom" } | { kind: "playbook" } | { kind: "builtin" };

export interface PipelineFact {
  value: string;
  why: string;
  tone: "agent" | "you" | "off";
  source: PipelineSource;
}

/** A repo's pipeline, every value from the layer that decides it (lands_pipeline.rs, NIT-492). */
export interface Pipeline {
  repo: string;
  mode: ReviewMode;
  checks: PipelineFact;
  review: PipelineFact;
  goals: PipelineFact;
  lands: PipelineFact;
  suites: PipelineFact;
  after: PipelineFact;
  warnings: string[];
}

/** What a change to one path would go through. */
export interface PathDryRun {
  path: string;
  checks: "refused" | "full" | "quick" | "basic" | "none";
  commands: string[];
  why: string;
  waits: string[];
}

export function sourceLabel(s: PipelineSource): string {
  switch (s.kind) {
    case "preset":
      return `${MODE_LABEL[s.mode]} preset`;
    case "custom":
      return "Custom";
    case "playbook":
      return "Playbook";
    case "builtin":
      return "Built-in";
  }
}

const CHECKS: Record<PathDryRun["checks"], string> = {
  refused: "would be refused",
  full: "would run the full gate",
  quick: "would run",
  basic: "would run",
  none: "would run no tests",
};

/** "A change to src/X would run npm test and npm run build", and what else would happen to it. */
export function dryRunLine(d: PathDryRun): string {
  const listed = d.checks === "quick" || d.checks === "basic" || (d.checks === "full" && d.commands.length > 0);
  const runs = `A change to ${d.path} ${CHECKS[d.checks]}${listed ? `${d.checks === "full" ? ":" : ""} ${d.commands.join(", ")}` : ""}.`;
  const waits = d.waits.length ? ` It waits for you: ${d.waits.join("; ")}.` : "";
  return `${runs} ${d.why}.${waits}`;
}
