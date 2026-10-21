import { useEffect, useState } from "react";
import { invoke } from "../backend";

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
  /** The projects that count (their keys); empty counts every project. */
  projects: string[];
  /** The ISO week of the last report the service took, if one was sent. */
  last_sent_week: string | null;
}

/** `leaderboard_status`, as much of it as the page reads. */
interface Status {
  joined: boolean;
  profile: { display_name: string; github?: string | null; x?: string | null; website?: string | null };
  projects: string[];
  last_sent_week?: string | null;
}

function profileOf(s: Status): Profile | null {
  if (!s.joined) return null;
  const p = s.profile;
  return { display_name: p.display_name, github: p.github ?? null, x: p.x ?? null, website: p.website ?? null, projects: s.projects, last_sent_week: s.last_sent_week ?? null };
}

// One copy for the whole app (the sidebar's tag and the page read the same), undefined until read.
let current: Profile | null | undefined;
let reading: Promise<void> | null = null;
const listeners = new Set<() => void>();
const changed = (p: Profile | null) => {
  current = p;
  listeners.forEach((l) => l());
};

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

function takeOld(): (Omit<Profile, "last_sent_week"> & { projects?: string[] }) | null {
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
export async function join(profile: { display_name: string; github: string | null; x: string | null; website: string | null }, projects: string[]): Promise<Profile | null> {
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
