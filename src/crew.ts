import { shortTokens } from "./spend.ts";
import type { CrewEdit, CrewStatus, CrewView } from "./types";

/**
 * What the crew control says next to "Crew n/cap" (docs/crew.md): nothing while it works, else
 * why it is not working, in words. `attention` when a person has to do something about it.
 */
export function crewWords(s: CrewStatus): { text: string; attention: boolean } | null {
  if (!s.supervised) return { text: "not running: start nitely crew run", attention: true };
  const said = s.status ?? "";
  if (s.paused) return { text: said.startsWith("paused") ? said : "paused", attention: said.startsWith("paused:") };
  if (!said || said === "working") return null;
  if (said.startsWith("working; nothing else ready")) return null;
  const attention = said.startsWith("no gate") || said.startsWith("no agent") || said.startsWith("could not") || said.startsWith("the playbook");
  return { text: said, attention };
}

/** The control's hover text: each running worker, and today's count against the cap. */
export function crewTitle(s: CrewStatus, now: number): string {
  const lines = s.running.map((w) => `${w.issue_key} · ${w.agent} · ${w.phase === "setup" ? "setting up its worktree since" : "started"} ${minutesAgo(now, w.started_at)}`);
  lines.push(`${s.started_today} of ${s.max_per_day} started today`);
  if (s.tokens_today !== undefined) {
    lines.push(`${shortTokens(s.tokens_today)}${s.max_tokens_per_day ? ` of ${shortTokens(s.max_tokens_per_day)}` : ""} tokens today`);
  }
  return lines.join("\n");
}

/** As `ago` in util.ts, which this file cannot import (it loads Tauri, and tests run in node). */
function minutesAgo(now: number, ts: number): string {
  const m = Math.max(0, Math.round((now - ts) / 60));
  return m < 60 ? `${m}m ago` : `${Math.round(m / 60)}h ago`;
}

/** The Crew section's form: quiet hours as two times and an on switch, the budget in millions. */
export interface CrewDraft {
  workers: number;
  agents: string[];
  maxPerDay: string;
  budgetMillions: string;
  quiet: boolean;
  quietFrom: string;
  quietTo: string;
}

export function crewDraftOf(v: CrewView): CrewDraft {
  const [from, to] = (v.quiet_hours ?? "").split("-").map((s) => s.trim());
  return {
    workers: v.workers,
    agents: v.agents.length ? [...v.agents] : v.choices.filter((c) => c.installed).map((c) => c.name),
    maxPerDay: v.max_per_day == null ? "" : String(v.max_per_day),
    // Exact, so a save that changes something else keeps the budget as it is.
    budgetMillions: v.max_tokens_per_day == null ? "" : String(v.max_tokens_per_day / 1_000_000),
    quiet: !!v.quiet_hours,
    quietFrom: from || "01:00",
    quietTo: to || "07:00",
  };
}

/** What the draft saves, or why it cannot be saved yet. */
export function crewEditOf(d: CrewDraft): CrewEdit | string {
  if (!Number.isInteger(d.workers) || d.workers < 0) return "Workers is a whole number, 0 for off.";
  if (d.workers > 0 && d.agents.length === 0) return "Pick at least one agent.";
  let max_per_day: number | null = null;
  if (d.maxPerDay.trim()) {
    const n = Number(d.maxPerDay);
    if (!Number.isInteger(n) || n < 1) return "Starts a day is a whole number, at least 1.";
    max_per_day = n;
  }
  let max_tokens_per_day: number | null = null;
  if (d.budgetMillions.trim()) {
    const m = Number(d.budgetMillions);
    if (!Number.isFinite(m) || m <= 0) return "The token budget is a number of millions, more than 0.";
    max_tokens_per_day = Math.round(m * 1_000_000);
  }
  const time = /^([01]\d|2[0-3]):[0-5]\d$/;
  if (d.quiet && (!time.test(d.quietFrom) || !time.test(d.quietTo))) return "Quiet hours need a start and an end.";
  if (d.quiet && d.quietFrom === d.quietTo) return "Quiet hours start and end at the same time.";
  return { workers: d.workers, agents: d.agents, max_per_day, max_tokens_per_day, quiet_hours: d.quiet ? `${d.quietFrom}-${d.quietTo}` : null };
}

/** The saved settings as the draft would save them, to tell whether anything changed. */
export function crewChanged(v: CrewView, d: CrewDraft): boolean {
  const e = crewEditOf(d);
  if (typeof e === "string") return true;
  const same = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);
  // A playbook that names no agents drafts the installed ones; that alone is no change.
  const agentsChanged = v.agents.length ? !same(e.agents, v.agents) : e.workers > 0 || !same(e.agents, crewDraftOf(v).agents);
  return (
    e.workers !== v.workers ||
    agentsChanged ||
    e.max_per_day !== v.max_per_day ||
    e.max_tokens_per_day !== v.max_tokens_per_day ||
    e.quiet_hours !== v.quiet_hours
  );
}
