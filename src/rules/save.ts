// How a Rules save handles a loosening (src/Rules.tsx), apart from React so node:test runs it
// (save.test.ts): a save that sends fewer changes to you never goes through until you have seen
// what loosens and confirmed exactly that.

/** A save's next step: confirm what loosens first, or done. */
export type SaveStep<T> = { loosens: string[] } | { saved: T };

/**
 * Checks the edit, then saves it. With nothing loosening it saves at once. Otherwise it saves,
 * confirmed, only when `confirmed` is the list the person was shown and the check still returns
 * that list; any other answer (a first save, an edit changed since) asks to confirm again.
 */
export async function confirmLoosening<T>(check: () => Promise<string[]>, save: (confirm: boolean) => Promise<T>, confirmed: string[] | null): Promise<SaveStep<T>> {
  const loosens = await check();
  if (loosens.length === 0) return { saved: await save(false) };
  const same = confirmed !== null && confirmed.length === loosens.length && confirmed.every((l, i) => l === loosens[i]);
  if (!same) return { loosens };
  return { saved: await save(true) };
}
