// A person's picture on the board (NIT-630): only the choice of one of their links travels, never
// an image or its address. The address is built here, from the handle, when the page renders.

export type AvatarChoice = "github" | "x";

/**
 * The user name in a link as people type it, read the way the report builder reads it
 * (leaderboard_report.rs): `@name`, `name`, `github.com/name`, `https://github.com/name/` all give
 * `name`. Null when what is left isn't a valid user name.
 */
function handleOf(v: string | null | undefined, prefixes: string[], valid: RegExp): string | null {
  let h = (v ?? "").trim().replace(/^@+/, "");
  for (const prefix of prefixes) if (h.startsWith(prefix)) h = h.slice(prefix.length).replace(/\/+$/, "");
  return valid.test(h) ? h : null;
}

/** The picture's address for `choice`, from the link's handle; null without a usable one. */
export function avatarUrl(choice: AvatarChoice | null | undefined, links: { github?: string | null; x?: string | null }): string | null {
  if (choice === "github") {
    const h = handleOf(links.github, ["https://github.com/", "http://github.com/", "github.com/"], /^[A-Za-z0-9-]{1,39}$/);
    return h && `https://github.com/${h}.png?size=96`;
  }
  if (choice === "x") {
    // X has no public picture address without its API: unavatar.io, a public avatar service,
    // fetches it. fallback=false makes a missing picture fail, so the initials show instead.
    const h = handleOf(links.x, ["https://x.com/", "http://x.com/", "x.com/", "https://twitter.com/", "twitter.com/"], /^[A-Za-z0-9_]{1,15}$/);
    return h && `https://unavatar.io/x/${h}?fallback=false`;
  }
  return null;
}

/** Up to two initials from a display name. */
export function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  const letters = (words.length > 1 ? [words[0], words[words.length - 1]] : words).map((w) => [...w][0] ?? "");
  return letters.join("").toUpperCase();
}
