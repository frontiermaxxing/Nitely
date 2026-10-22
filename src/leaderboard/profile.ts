import { useEffect, useState } from "react";
import { invoke } from "../backend";
import type { AvatarChoice } from "./Avatar";

// Your leaderboard profile: whether you joined, the name the board shows, your public links and
// the projects that count. It lives in Nitely's data folder (crates/nitely-core/src/leaderboard_send.rs),
// read and written through the leaderboard_status, leaderboard_join and leaderboard_delete commands.
// Joining sends nothing; sending is a step of its own.

/** What the board shows for you and what counts, once you joined. */
export interface Profile {
  /** The name the board shows: not your name or git email. */
  display_name: string;
  github: string | null;
  x: string | null;
  website: string | null;
  /** Which link gives the board your picture, if any: only the choice is kept and sent. */
  avatar: AvatarChoice | null;
  /** The projects that count (their keys); empty counts every project. */
  projects: string[];
  /** The ISO week of the last report the service took, if one was sent. */
  last_sent_week: string | null;
  /** Why the last send or delete failed, if it did. */
  last_error: string | null;
  /** This install's id on the board (its public key), to find your row. */
  id: string | null;
  /** When the next send goes (seconds); null before the first send by hand. */
  next_send_at: number | null;
  /** When this week's entry closes: its Sunday-evening send (seconds). */
  week_closes_at: number | null;
  /** Your public page (NIT-632), once a report is on the board; null before. */
  page: string | null;
}

/** `leaderboard_status`, as much of it as the page reads. */
interface Status {
  joined: boolean;
  profile: { display_name: string; github?: string | null; x?: string | null; website?: string | null; avatar?: AvatarChoice | null };
  projects: string[];
  last_sent_week?: string | null;
  last_error?: string | null;
  id?: string | null;
  next_send_at?: number | null;
  week_closes_at?: number | null;
  /** The Leaderboard service this install sends to. */
  url?: string;
}

/** The public service; its people's pages are on nitely.net, which passes /u/ on to it. */
const PUBLIC_SERVICE = "https://api.nitely.net";

/**
 * Where your page is: nitely.net/u/<id> on the public service, and on a service of your own its
 * own /u/<id>, which it serves itself. Null until a report of yours is on the board.
 */
export function pageOf(url: string | undefined, id: string | null | undefined, sent: boolean): string | null {
  if (!id || !sent) return null;
  const service = (url ?? PUBLIC_SERVICE).trim().replace(/\/+$/, "");
  return service === PUBLIC_SERVICE ? `https://nitely.net/u/${id}` : `${service}/u/${id}`;
}

function profileOf(s: Status): Profile | null {
  if (!s.joined) return null;
  const p = s.profile;
  return {
    display_name: p.display_name,
    github: p.github ?? null,
    x: p.x ?? null,
    website: p.website ?? null,
    avatar: p.avatar ?? null,
    projects: s.projects,
    last_sent_week: s.last_sent_week ?? null,
    last_error: s.last_error ?? null,
    id: s.id ?? null,
    next_send_at: s.next_send_at ?? null,
    week_closes_at: s.week_closes_at ?? null,
    page: pageOf(s.url, s.id, !!s.last_sent_week),
  };
}

// One copy for the whole app (the sidebar's tag and the page read the same), undefined until read.
let current: Profile | null | undefined;
let reading: Promise<void> | null = null;
const listeners = new Set<() => void>();
const changed = (p: Profile | null) => {
  current = p;
  listeners.forEach((l) => l());
  readAgainLater(p);
};

// The app sends on its own once a send is due (src-tauri's hourly check), and nothing tells this
// page: so the status is read again a few minutes after the next send is due, and every few
// minutes while it stays due, and "next send" moves on once it went.
let again: ReturnType<typeof setTimeout> | undefined;
function readAgainLater(p: Profile | null) {
  clearTimeout(again);
  if (!p?.next_send_at) return;
  const wait = Math.max(p.next_send_at * 1000 - Date.now(), 0) + 5 * 60_000;
  again = setTimeout(
    () => {
      // Nobody is looking: the next look reads it fresh.
      if (listeners.size) read();
      else current = undefined;
    },
    Math.min(wait, 2 ** 31 - 1),
  );
}

/** Where an earlier build of this page kept your choices; moved into the data folder once. */
const OLD_KEY = "nitely.leaderboard.profile";

function read(): Promise<void> {
  reading ??= invoke<Status>("leaderboard_status")
    .then(async (s) => {
      let profile = profileOf(s);
      const old = takeOld();
      if (!profile && old) profile = await join(old, old.projects ?? []).catch(() => null);
      changed(profile);
    })
    .catch(() => changed(null))
    .finally(() => (reading = null));
  return reading;
}

function takeOld(): (Omit<Profile, "last_sent_week" | "last_error" | "id"> & { projects?: string[] }) | null {
  try {
    const raw = localStorage.getItem(OLD_KEY);
    localStorage.removeItem(OLD_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

/** Your profile: undefined while it is read, null before you join. */
export function useProfile(): Profile | null | undefined {
  const [profile, setProfile] = useState(current);
  useEffect(() => {
    const update = () => setProfile(current);
    listeners.add(update);
    if (current === undefined) read();
    else update();
    return () => {
      listeners.delete(update);
    };
  }, []);
  return profile;
}

/** Joins, or saves changed choices. Sends nothing; refuses a name or link the report would refuse. */
export async function join(
  profile: { display_name: string; github: string | null; x: string | null; website: string | null; avatar?: AvatarChoice | null },
  projects: string[],
): Promise<Profile | null> {
  const s = await invoke<Status>("leaderboard_join", { profile, projects });
  const p = profileOf(s);
  changed(p);
  return p;
}

/**
 * Leaves: deletes what this install sent from the board (only if it sent something), then forgets
 * the profile and the signing key. Resolves to how many reports the service deleted.
 */
export async function leave(): Promise<number> {
  const deleted = await invoke<number>("leaderboard_delete", { leave: true });
  changed(null);
  return deleted;
}

/** Whether this install is on the board: it sent a report, so the daily send goes on. */
export function sharing(p: Profile): boolean {
  return p.last_sent_week !== null;
}

/**
 * Sends this week's report now, by your hand; after the first, it goes once a day on its own, and
 * Sunday evening's closes the week.
 * A request to the leaderboard's service.
 */
export async function sendNow(): Promise<void> {
  try {
    await invoke("leaderboard_send");
  } finally {
    await read();
  }
}

/** Stops sharing: deletes every report this install sent from the board and stays joined, so nothing more goes until you send again. */
export async function stopSharing(): Promise<number> {
  try {
    return await invoke<number>("leaderboard_delete", { leave: false });
  } finally {
    await read();
  }
}
