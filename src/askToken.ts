/** The ask an `ask:<n>` token names (`http://127.0.0.1:7423/a/69` opens one), or null for anything else. */
export function askIdOf(token: string | null): number | null {
  const m = token?.match(/^ask:(\d+)$/i);
  return m ? Number(m[1]) : null;
}
