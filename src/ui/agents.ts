import type { Session } from "../types";

/** A session quiet for this long reads as stale even if it never sent an end event. */
export const STALE_SECS = 30 * 60;
/** A session waiting on the human for this long is a long wait: shown louder, never quieter. */
export const LONG_WAIT_SECS = 15 * 60;

/** What an agent session looks like right now: the session states plus "stale". */
export type Liveness = "working" | "waiting" | "idle" | "stale" | "ended";

/** The one rule for every view: a session's state, with quiet ones turned stale. */
export function livenessFrom(state: string, lastSeen: number, now: number): Liveness {
  if (state === "ended") return "ended";
  // A waiting session sends nothing while it waits, so silence there is the human's, not the
  // agent's: it stays waiting however long it takes (waitedLong makes it louder instead).
  if (state === "waiting") return "waiting";
  if (now - lastSeen > STALE_SECS) return "stale";
  return state === "working" || state === "idle" ? state : "idle";
}

export function livenessOf(session: Session | null | undefined, now: number): Liveness | null {
  return session ? livenessFrom(session.state, session.last_seen_at, now) : null;
}

/** Waiting on the human for `LONG_WAIT_SECS` or more. */
export function waitedLong(session: { state: string; last_seen_at: number } | null | undefined, now: number): boolean {
  return session?.state === "waiting" && now - session.last_seen_at >= LONG_WAIT_SECS;
}

/** The liveness in words; a long wait says how long: "waiting for you 40m". */
export function livenessLabel(session: Session | null | undefined, now: number): string {
  const live = livenessOf(session, now);
  if (!live) return "no session";
  if (!waitedLong(session, now)) return LIVENESS_LABEL[live];
  const mins = Math.round((now - session!.last_seen_at) / 60);
  return `${LIVENESS_LABEL.waiting} ${mins < 120 ? `${mins}m` : `${Math.round(mins / 60)}h`}`;
}

export const LIVENESS_LABEL: Record<Liveness, string> = {
  working: "working",
  waiting: "waiting for you",
  idle: "idle",
  stale: "stale",
  ended: "ended",
};

export type Vendor = "claude" | "codex" | "cursor" | "human" | "other";

/** `claude:c:/code/nitely` or `claude` → `claude`; anything unknown → `other`. */
export function vendorOf(actorOrVendor: string | null | undefined): Vendor {
  // "claude:c:/repo/wt" here, "claude@mac" from another machine.
  const v = (actorOrVendor ?? "").split(/[:@]/)[0].toLowerCase();
  return v === "claude" || v === "codex" || v === "cursor" || v === "human" ? v : "other";
}

export const VENDOR_LABEL: Record<Vendor, string> = {
  claude: "Claude",
  codex: "Codex",
  cursor: "Cursor",
  human: "You",
  other: "Agent",
};
