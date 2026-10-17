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

/** How an agent is drawn and named. */
export interface VendorInfo {
  /** Short name, in lines like "Claude passed". */
  label: string;
  /** The product, where people pick which agents to connect. */
  product: string;
  /**
   * Its mark inside the rounded square: two letters, or an SVG path on a 16 by 16 grid for a
   * product mark its owner lets us show (src/ui/vendors/README.md says which, and why not yet).
   */
  mark: { letters: string } | { path: string };
  /** Its colour token, defined for both themes in tokens.css. */
  color: string;
}

/**
 * Every agent Nitely knows. Adding one (Gemini CLI, GitHub Copilot) is an entry here and its
 * `--vendor-*` colour in tokens.css; vendorOf, the labels and AgentMark follow.
 */
export const VENDORS = {
  claude: { label: "Claude", product: "Claude Code", mark: { letters: "Cl" }, color: "var(--vendor-claude)" },
  codex: { label: "Codex", product: "Codex", mark: { letters: "Cx" }, color: "var(--vendor-codex)" },
  cursor: { label: "Cursor", product: "Cursor", mark: { letters: "Cu" }, color: "var(--vendor-cursor)" },
} satisfies Record<string, VendorInfo>;

export type Agent = keyof typeof VENDORS;
/** An agent, the human (a ring, never one more agent) or something unknown. */
export type Vendor = Agent | "human" | "other";

const isAgent = (v: string): v is Agent => Object.prototype.hasOwnProperty.call(VENDORS, v);

/** `claude:c:/code/nitely` or `claude` → `claude`; anything unknown → `other`. */
export function vendorOf(actorOrVendor: string | null | undefined): Vendor {
  // "claude:c:/repo/wt" here, "claude@mac" from another machine.
  const v = (actorOrVendor ?? "").split(/[:@]/)[0].toLowerCase();
  return isAgent(v) || v === "human" ? v : "other";
}

export const VENDOR_LABEL: Record<Vendor, string> = {
  ...(Object.fromEntries(Object.entries(VENDORS).map(([k, v]) => [k, v.label])) as Record<Agent, string>),
  human: "You",
  other: "Agent",
};

/** What draws a vendor: its entry, or the neutral one for an unknown agent. */
export function vendorInfo(vendor: Vendor): VendorInfo | null {
  if (vendor === "human") return null;
  return vendor === "other" ? { label: "Agent", product: "Agent", mark: { letters: "?" }, color: "var(--vendor-other)" } : VENDORS[vendor];
}

/**
 * An agent's short, stable name. A session (`claude:<session id>`, `codex:<thread id>`) is its vendor
 * and the last four characters of its id, `Claude · 3317`, the same wherever it shows; an agent
 * known only by its folder (`claude:c:/code/nitely`, older events) is its vendor and the folder's
 * name, `Claude · nitely`.
 */
export function agentLabel(id: string): string {
  const [vendor, ...rest] = id.split(":");
  const tail = rest.join(":");
  const name = vendor ? vendor[0].toUpperCase() + vendor.slice(1) : vendor;
  if (isSessionId(tail)) return `${name} · ${tail.replace(/[^0-9a-z]/gi, "").slice(-4).toLowerCase()}`;
  const last = tail.split("/").filter(Boolean).pop();
  return last ? `${name} · ${last}` : name;
}

/** The hover text for an agent: its full id, and the folder its session works in when known. */
export function agentTitle(id: string, place?: string | null): string {
  const label = agentLabel(id);
  const full = label === id ? id : `${label}: ${id}`;
  return place ? `${full}, in ${place}` : full;
}

/** A session or thread id (a UUID, or another opaque id), not a folder. */
function isSessionId(tail: string): boolean {
  return tail.length >= 8 && !/[\\/]/.test(tail) && /^[0-9a-z-]+$/i.test(tail);
}
