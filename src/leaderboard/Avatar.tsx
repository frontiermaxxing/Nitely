import { useState } from "react";
import { avatarUrl, initials, type AvatarChoice } from "./picture";

// A person's picture on the board (NIT-630). Its address comes from picture.ts, built from the
// handle when the page renders; the image loads straight from that site without a referrer.
// Without a choice, or if the image fails, their initials stand in.

export type { AvatarChoice };

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
