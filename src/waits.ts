// What every waiting request waits for, as the app says it (NIT-322): the Waiting list on Now and
// Overview, and the line on an issue's card and panel. The backend says why (waits.rs); this words
// what kind of request it is and how long things have been held.

import type { WaitBehind, Waiting, Waits } from "./types";

export const WAIT_KIND: Record<string, string> = {
  run: "Run",
  gate: "Gate",
  land: "Land gate",
  submit: "Submit",
  review: "Review",
  land_queue: "Land",
  land_refused: "Land",
  landing: "Landing",
};

/** "40 s", "6 m", "1 h 4 m": how long since `since`, short. */
export function held(now: number, since: number | null): string | null {
  if (since === null) return null;
  const s = Math.max(0, Math.round(now - since));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} m`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return m ? `${h} h ${m} m` : `${h} h`;
}

/** "run 412: NIT-290 gate, 6 m". */
export function behindLine(b: WaitBehind, now: number): string {
  const age = held(now, b.since);
  return age ? `${b.what}, ${age}` : b.what;
}

/** What the request waits behind, in one line: the first two, then how many more. */
export function behindSummary(w: Waiting, now: number): string | null {
  if (w.behind.length === 0) return null;
  const first = w.behind.slice(0, 2).map((b) => behindLine(b, now));
  const more = w.behind.length - first.length;
  return `behind ${first.join("; ")}${more > 0 ? `; ${more} more` : ""}`;
}

/** The longest-waiting request of the issue, if one waits. */
export function waitOf(key: string, waits: Waits | null): Waiting | null {
  return waits?.waiting.find((w) => w.issue?.toUpperCase() === key.toUpperCase()) ?? null;
}

/**
 * "Gate waiting: all 2 of its lanes are busy in NIT", "Gate: waiting for memory: ...": the issue's
 * card and panel line, saying "waiting" once.
 */
export function waitLine(w: Waiting): string {
  const kind = WAIT_KIND[w.kind] ?? w.kind;
  // The land holding the lock is not waiting: it is what others wait for.
  if (w.kind === "landing") return `Landing now: other lands of this repo wait for it`;
  return /^wait/i.test(w.why) ? `${kind}: ${w.why}` : `${kind} waiting: ${w.why}`;
}
