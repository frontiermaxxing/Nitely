// Which decision the phone shows after a refresh (src/phone/Inbox.tsx): the one it showed, by id,
// wherever it moved; when that one is gone, whatever now sits at its last position, so the one
// after it takes its place.

export interface Shown {
  id: string | null;
  /** Its position the last time it was on screen. */
  index: number;
}

export function shownIndex(ids: string[], shown: Shown): number {
  const at = shown.id === null ? -1 : ids.indexOf(shown.id);
  return at >= 0 ? at : Math.min(shown.index, Math.max(0, ids.length - 1));
}
