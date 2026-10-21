// Your leaderboard profile: whether you joined, the name the board shows and your public links.
// It is kept on this machine only (this app's storage); joining sends nothing, and sending is a
// step of its own that comes with the leaderboard's service.

export interface Profile {
  /** The name the board shows: not your name or git email. */
  display_name: string;
  github: string | null;
  x: string | null;
  website: string | null;
  /** The projects that count (their keys); empty counts every project. */
  projects: string[];
  /** When you joined, seconds since the epoch. */
  joined_at: number;
}

const KEY = "nitely.leaderboard.profile";
const CHANGED = "nitely:leaderboard-profile";

export function loadProfile(): Profile | null {
  try {
    const raw = localStorage.getItem(KEY);
    return raw ? (JSON.parse(raw) as Profile) : null;
  } catch {
    return null;
  }
}

/** Saves it (null: leaves), and tells the sidebar. */
export function saveProfile(profile: Profile | null) {
  if (profile) localStorage.setItem(KEY, JSON.stringify(profile));
  else localStorage.removeItem(KEY);
  window.dispatchEvent(new Event(CHANGED));
}

/** Whether you joined the leaderboard; nothing is shared before you do. */
export function leaderboardJoined(): boolean {
  return loadProfile() !== null;
}

/** Calls `onChange` when the profile is saved or cleared, here or in another window. */
export function onProfileChange(onChange: () => void): () => void {
  const storage = (e: StorageEvent) => e.key === KEY && onChange();
  window.addEventListener(CHANGED, onChange);
  window.addEventListener("storage", storage);
  return () => {
    window.removeEventListener(CHANGED, onChange);
    window.removeEventListener("storage", storage);
  };
}
