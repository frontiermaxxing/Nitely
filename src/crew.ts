import type { CrewStatus } from "./types";

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
  const lines = s.running.map((w) => `${w.issue_key} · ${w.agent} · started ${minutesAgo(now, w.started_at)}`);
  lines.push(`${s.started_today} of ${s.max_per_day} started today`);
  return lines.join("\n");
}

/** As `ago` in util.ts, which this file cannot import (it loads Tauri, and tests run in node). */
function minutesAgo(now: number, ts: number): string {
  const m = Math.max(0, Math.round((now - ts) / 60));
  return m < 60 ? `${m}m ago` : `${Math.round(m / 60)}h ago`;
}
