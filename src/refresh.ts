// How often the app reads the board again (NIT-337). A snapshot runs git in every worktree and can
// take seconds, and the database changes several times a minute while agents work, so the board is
// read again no sooner than its last read took (it never runs more than half the time), and while
// the window is hidden at most every HIDDEN_GAP_MS (notifications still need the board).

/** The least time between two board reads. */
export const REFRESH_GAP_MS = 1000;
/** The least time between two board reads while the window is hidden. */
export const HIDDEN_GAP_MS = 15000;

/** How long to wait after a board read that took `lastMs` before the next one. */
export function refreshGap(lastMs: number, hidden: boolean): number {
  return Math.max(REFRESH_GAP_MS, lastMs, hidden ? HIDDEN_GAP_MS : 0);
}

/** Milliseconds still to wait before the next read: 0 when it may run now. */
export function waitLeft(now: number, lastEnd: number | null, lastMs: number, hidden: boolean): number {
  if (lastEnd === null) return 0;
  return Math.max(0, lastEnd + refreshGap(lastMs, hidden) - now);
}
