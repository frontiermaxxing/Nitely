import type { Session } from "../types";

/** A session quiet for this long reads as stale even if it never sent an end event. */
export const STALE_SECS = 30 * 60;

/** What an agent session looks like right now: the session states plus "stale". */
export type Liveness = "working" | "waiting" | "idle" | "stale" | "ended";

export function livenessOf(session: Session | null | undefined, now: number): Liveness | null {
  if (!session) return null;
  if (session.state === "ended") return "ended";
  if (now - session.last_seen_at > STALE_SECS) return "stale";
  return session.state;
}

export const LIVENESS_LABEL: Record<Liveness, string> = {
  working: "working",
  waiting: "waiting for you",
  idle: "idle",
  stale: "stale",
  ended: "ended",
};

export type Vendor = "claude" | "codex" | "cursor" | "human" | "other";

/** `claude:c:/development/nitely` or `claude` → `claude`; anything unknown → `other`. */
export function vendorOf(actorOrVendor: string | null | undefined): Vendor {
  const v = (actorOrVendor ?? "").split(":")[0].toLowerCase();
  return v === "claude" || v === "codex" || v === "cursor" || v === "human" ? v : "other";
}

export const VENDOR_LABEL: Record<Vendor, string> = {
  claude: "Claude",
  codex: "Codex",
  cursor: "Cursor",
  human: "You",
  other: "Agent",
};
