// Telling the person, outside the window, that a decision waits on them. The decisions are the
// Inbox's own (decisionsOf in src/decisions.ts), so a notification and the Inbox never disagree.
// This file decides what is new and what to say; useNotifications (src/App.tsx) batches it and
// src-tauri/src/notify.rs shows it.

import { projectOf, sessionOf, type Decision } from "./decisions.ts";
import type { IssueDetail } from "./types";
import { VENDOR_LABEL, vendorOf } from "./ui/agents.ts";

/** What a notification can be about; each can be turned off. */
export type NotifyKind = "ask" | "approval" | "crew" | "land" | "waiting";

export const NOTIFY_KINDS: { kind: NotifyKind; label: string }[] = [
  { kind: "ask", label: "An agent asks you something" },
  { kind: "approval", label: "An issue waits for your approval" },
  { kind: "crew", label: "The crew gave up on an issue" },
  { kind: "land", label: "A land stopped, on a conflict or a failure" },
  { kind: "waiting", label: "An agent waits for input in its terminal" },
];

export interface NotifySettings {
  kinds: Record<NotifyKind, boolean>;
  /** Nothing notifies between `from` and `to` (local "HH:MM"; across midnight when from > to). */
  quiet: { on: boolean; from: string; to: string };
}

export const DEFAULT_SETTINGS: NotifySettings = {
  kinds: { ask: true, approval: true, crew: true, land: true, waiting: true },
  quiet: { on: false, from: "22:00", to: "08:00" },
};

/** When each decision was last seen waiting (unix seconds), by decision id. */
export type Seen = Record<string, number>;

/** A decision that went away comes back as a new one only after this long (a stale board can drop and re-add it). */
export const FORGET_SECS = 10 * 60;

/**
 * What "seen" remembers a decision by: its id, except that an issue waiting on the person is one
 * thing whether it shows as an approval or as their own work (it can change from one to the other
 * once its detail is read).
 */
export const seenId = (d: Decision) => (d.issue && (d.kind === "approve" || d.kind === "own") ? `waits-${d.issue.key}` : d.id);

/** True when the issue reached Needs You because the crew stopped trying (crates/nitely-crew/src/crew.rs). */
function crewGaveUp(detail: IssueDetail | undefined): boolean {
  const moved = detail?.timeline.filter((e) => e.kind === "issue_moved" && e.summary?.includes("→ Needs You")).pop();
  return !!moved?.summary && /the crew (tried it|could not)/.test(moved.summary);
}

/** The kind a decision notifies as, or null when it never does (the person's own work is theirs to pick up). */
export function kindOf(d: Decision, detail?: IssueDetail): NotifyKind | null {
  switch (d.kind) {
    case "ask":
      return "ask";
    case "approve":
      return crewGaveUp(detail) ? "crew" : "approval";
    case "conflict":
    case "land":
      return "land";
    case "input":
      return "waiting";
    case "own":
      return null;
  }
}

/** The one line a notification says about a decision: which issue, and what it needs. */
export function lineOf(d: Decision, kind: NotifyKind): string {
  const key = d.issue?.key ?? d.ask?.issue_key ?? d.row?.branch ?? d.repo ?? "An agent";
  switch (kind) {
    case "ask":
      return `${key} asks: ${d.ask?.question ?? "a question"}`;
    case "approval":
      return `${key} waits for your approval: ${d.issue?.title ?? ""}`.replace(/: $/, "");
    case "crew":
      return `The crew gave up on ${key}: ${d.issue?.title ?? ""}`.replace(/: $/, "");
    case "land":
      return d.kind === "conflict" ? `${key}: the rebase stopped on a conflict` : `${key}: the land stopped`;
    case "waiting":
      return `${VENDOR_LABEL[vendorOf(sessionOf(d)?.vendor ?? d.row?.vendor ?? null)]} waits for input on ${key}`;
  }
}

const TITLE: Record<NotifyKind, string> = {
  ask: "A question for you",
  approval: "Waiting for your approval",
  crew: "The crew needs you",
  land: "A land stopped",
  waiting: "An agent is waiting",
};

/** True inside the quiet hours. */
export function inQuiet(settings: NotifySettings, at: Date): boolean {
  if (!settings.quiet.on) return false;
  const mins = (hhmm: string) => {
    const [h, m] = hhmm.split(":").map(Number);
    return (h || 0) * 60 + (m || 0);
  };
  const now = at.getHours() * 60 + at.getMinutes();
  const from = mins(settings.quiet.from);
  const to = mins(settings.quiet.to);
  return from <= to ? now >= from && now < to : now >= from || now < to;
}

export interface Fresh {
  decision: Decision;
  kind: NotifyKind;
  line: string;
}

/**
 * The decisions that are new since `seen` and should notify, and the updated `seen`. A decision
 * notifies once: the first time it is seen waiting. On the first run (`seen` is null) everything
 * already waiting is taken as seen, so starting the app never floods. Nothing notifies while the
 * person is looking at the Inbox, in quiet hours, or for a kind they turned off; those count as
 * seen too, so they do not notify later either.
 */
export function freshDecisions(
  decisions: Decision[],
  details: Map<string, IssueDetail>,
  seen: Seen | null,
  settings: NotifySettings,
  now: number,
  opts: { inboxInView: boolean; at?: Date; inboxProject?: string | null },
): { fresh: Fresh[]; seen: Seen } {
  const next: Seen = {};
  for (const [id, ts] of Object.entries(seen ?? {})) if (now - ts < FORGET_SECS) next[id] = ts;
  const quiet = inQuiet(settings, opts.at ?? new Date(now * 1000));
  const fresh: Fresh[] = [];
  for (const d of decisions) {
    const id = seenId(d);
    const known = seen !== null && id in next;
    // An issue waiting on the person is told as what it is only once its detail is read (whether
    // the crew gave up; whether it is a playbook change to approve or their own work): until then
    // it waits unseen.
    if (!known && seen !== null && (d.kind === "approve" || d.kind === "own") && d.issue && !details.has(d.issue.key)) continue;
    next[id] = now;
    if (seen === null || known || inView(d, opts) || quiet) continue;
    const kind = kindOf(d, d.issue ? details.get(d.issue.key) : undefined);
    if (!kind || !settings.kinds[kind]) continue;
    fresh.push({ decision: d, kind, line: lineOf(d, kind) });
  }
  return { fresh, seen: next };
}

/**
 * Whether the person has the decision in front of them: the Inbox is in view and lists it. With the
 * Inbox filtered to a project (NIT-236), another project's decisions are not in view, so they still
 * notify.
 */
export function inView(d: Decision, opts: { inboxInView: boolean; inboxProject?: string | null }): boolean {
  return opts.inboxInView && (!opts.inboxProject || projectOf(d)?.toUpperCase() === opts.inboxProject.toUpperCase());
}

/** One notification for a batch: the decision itself when it is one, else how many and the first two. */
export function messageOf(batch: Fresh[]): { title: string; body: string; decision: string | null } {
  if (batch.length === 1) return { title: TITLE[batch[0].kind], body: batch[0].line, decision: batch[0].decision.id };
  const more = batch.length > 2 ? `\nand ${batch.length - 2} more` : "";
  return { title: `${batch.length} decisions waiting`, body: batch.slice(0, 2).map((f) => f.line).join("\n") + more, decision: null };
}
