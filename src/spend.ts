// Spend in words (crates/nitely-core/src/usage.rs): tokens, never dollars. Pure, so node:test
// can load it.

import type { IssueDetail, IssueSpend, ModelTokens, ProjectSpend } from "./types";
import { automatedReviewEvidence } from "./approvalEvidence.ts";

/** A token count in a few characters, as the backend writes it: 512, 86k, 1.2M. */
export function shortTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  const m = n / 1e6;
  return m < 10 ? `${m.toFixed(1)}M` : `${Math.round(m)}M`;
}

/** A model's name without its vendor: claude-opus-5-5 is opus-5-5. */
export function modelName(model: string): string {
  return model.replace(/^claude-/, "") || "unknown model";
}

function span(secs: number): string {
  const m = Math.round(secs / 60);
  if (m < 60) return `${Math.max(1, m)}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h ${m % 60}m` : `${Math.floor(h / 24)}d ${h % 24}h`;
}

const models = (by: ModelTokens[]) =>
  by.length > 1 ? ` (${by.map((m) => `${modelName(m.model)} ${shortTokens(m.tokens)}`).join(", ")})` : by.length === 1 ? ` on ${modelName(by[0].model)}` : "";

/**
 * The issue's spend in one line: how long from the start of work to the land (or so far), the
 * tokens its agents used by model, model calls, and review rounds. Parts that are not known are
 * left out; null when nothing is.
 */
export function issueSpendLine(detail: IssueDetail, spend: IssueSpend | null, now: number): string | null {
  const t = detail.timeline;
  const started = t.find((e) => e.kind === "worktree" || e.kind === "claimed" || (e.kind === "issue_moved" && /→ In Progress/.test(e.summary ?? "")));
  const landed = [...t].reverse().find((e) => e.kind === "landed");
  const parts: string[] = [];
  if (started) parts.push(landed ? `${span(landed.ts - started.ts)} from start to land` : `${span(now - started.ts)} so far`);
  if (spend && spend.tokens > 0) {
    parts.push(`${shortTokens(spend.tokens)} tokens${models(spend.by_model)}`);
    parts.push(`${spend.calls} model call${spend.calls === 1 ? "" : "s"}`);
  }
  const rounds = detail.reviews.filter((review) => automatedReviewEvidence(review).ran).length;
  if (rounds > 0) parts.push(`${rounds} review round${rounds === 1 ? "" : "s"}`);
  return parts.length ? parts.join(" · ") : null;
}

/** The project's spend in its summary: the window's tokens, then per landed issue. */
export function projectSpendWords(s: ProjectSpend): { total: string; perLanded: string; title: string } {
  const lines = [
    `${shortTokens(s.tokens)} tokens in ${s.days.length} days (${shortTokens(s.cache_read)} more read from cache)`,
    ...s.by_agent.map((a) => `${a.vendor}: ${shortTokens(a.tokens)}`),
    ...s.by_model.map((m) => `  ${modelName(m.model)}: ${shortTokens(m.tokens)} in ${m.calls} calls`),
    `${s.landed} landed${s.per_landed ? `, ${shortTokens(s.per_landed)} tokens per landed issue` : ""}`,
    ...(s.top_issues.length ? ["Most tokens:", ...s.top_issues.map((i) => `  ${i.key}: ${shortTokens(i.tokens)}`)] : []),
  ];
  return { total: `${shortTokens(s.tokens)} tokens`, perLanded: s.per_landed ? `${shortTokens(s.per_landed)} per landed issue` : "", title: lines.join("\n") };
}
