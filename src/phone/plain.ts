// Agents write a little markdown in questions and findings: `code`, **bold**, and issue links
// such as [NIT-5](nitely://issue/NIT-5), which the phone cannot open. Where the phone shows text
// without formatting (buttons, notifications), it shows what a reader would see.
import { parseInline } from "../markdown.ts";

/** The text as read: links as their text (an issue link as its key), marks dropped. */
export function plain(s: string): string {
  return parseInline(s)
    .map((p) => p.s)
    .join("");
}
