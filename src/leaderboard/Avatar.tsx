import { useState } from "react";

// A person's picture on the board (NIT-630): only the choice of one of their links travels, never
// an image or its address. The address is built here, from the handle, when the page renders, and
// the image loads straight from that site without a referrer. Without a choice, or if the image
// fails, their initials stand in.

export type AvatarChoice = "github" | "x";

/** The picture's address for `choice`, from the link's handle; null without a usable one. */
export function avatarUrl(choice: AvatarChoice | null | undefined, links: { github?: string | null; x?: string | null }): string | null {
  if (choice === "github") {
    const h = links.github?.replace(/^https:\/\/github\.com\//, "");
    return h && /^[A-Za-z0-9-]{1,39}$/.test(h) ? `https://github.com/${h}.png?size=96` : null;
  }
  if (choice === "x") {
    // X has no public picture address without its API: unavatar.io, a public avatar service,
    // fetches it. fallback=false makes a missing picture fail, so the initials show instead.
    const h = links.x?.replace(/^https:\/\/x\.com\//, "").replace(/^@/, "");
    return h && /^[A-Za-z0-9_]{1,15}$/.test(h) ? `https://unavatar.io/x/${h}?fallback=false` : null;
  }
  return null;
}

/** Up to two initials from a display name. */
export function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  const letters = (words.length > 1 ? [words[0], words[words.length - 1]] : words).map((w) => [...w][0] ?? "");
  return letters.join("").toUpperCase();
}

export function Avatar(props: { name: string; choice?: AvatarChoice | null; links: { github?: string | null; x?: string | null }; size: number }) {
  const url = avatarUrl(props.choice, props.links);
  const [failed, setFailed] = useState<string | null>(null);
  const style = { width: props.size, height: props.size, fontSize: Math.round(props.size * 0.4) };
  if (url && failed !== url)
    return (
      <img
        className="lb-avatar"
        src={url}
        alt=""
        width={props.size}
        height={props.size}
        style={style}
        referrerPolicy="no-referrer"
        loading="lazy"
        decoding="async"
        onError={() => setFailed(url)}
      />
    );
  return (
    <span className="lb-avatar lb-avatar-initials" style={style} aria-hidden="true">
      {initials(props.name)}
    </span>
  );
}
