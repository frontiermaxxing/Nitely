// Which decision the phone shows after a refresh (src/phone/Inbox.tsx): the one it showed, by id,
// wherever it moved; when that one is gone, whatever now sits at its last position, so the one
// after it takes its place.

export interface Shown {
  id: string | null;
  /** Its position the last time it was on screen. */
  index: number;
}

/** "NIT" for "NIT-12"; null for no key. */
export function keyProject(key: string | null | undefined): string | null {
  return key?.match(/^([A-Za-z][A-Za-z0-9]*)-\d+$/)?.[1].toUpperCase() ?? null;
}

/**
 * The phone's project filter (NIT-236), as the app's Inbox has it: each project with decisions
 * waiting and how many, most first; and the project still filtered to, or null (All) once it has
 * nothing left.
 */
export function phoneProjects(keys: (string | null | undefined)[], project: string | null): { counts: { project: string; count: number }[]; project: string | null } {
  const counts = new Map<string, number>();
  for (const k of keys) {
    const p = keyProject(k);
    if (p) counts.set(p, (counts.get(p) ?? 0) + 1);
  }
  const list = [...counts].map(([p, count]) => ({ project: p, count })).sort((a, b) => b.count - a.count || a.project.localeCompare(b.project));
  const kept = project && counts.has(project.toUpperCase()) ? project.toUpperCase() : null;
  return { counts: list, project: kept };
}

export function shownIndex(ids: string[], shown: Shown): number {
  const at = shown.id === null ? -1 : ids.indexOf(shown.id);
  return at >= 0 ? at : Math.min(shown.index, Math.max(0, ids.length - 1));
}
