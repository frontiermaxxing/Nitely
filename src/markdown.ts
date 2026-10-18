/**
 * Just enough Markdown for a project brief: headings, paragraphs, lists (nested by indent, with
 * task boxes), quotes, rules, fenced code and pipe tables; inline code, bold, italics and links.
 * It parses to plain data, so the page renders React elements and never sets HTML.
 */

export type Inline =
  | { t: "text"; s: string }
  | { t: "code"; s: string }
  | { t: "strong"; s: string }
  | { t: "em"; s: string }
  | { t: "link"; s: string; href: string };

/**
 * The issue key a link names, in either form agents write: `nitely://issue/NIT-12`, or the app's
 * loopback web form `http://127.0.0.1:7423/i/NIT-12` (crates/nitely-core/src/links.rs). Inside the
 * app both open the issue's panel; elsewhere the web form is the one chats and GitHub open.
 */
export function issueKeyOf(href: string): string | null {
  const m =
    /^nitely:\/\/issue\/([A-Za-z][A-Za-z0-9]*-\d+)\/?$/.exec(href.trim()) ??
    /^http:\/\/(?:127\.0\.0\.1|localhost):\d+\/i\/([A-Za-z][A-Za-z0-9]*-\d+)\/?$/.exec(href.trim());
  return m ? m[1].toUpperCase() : null;
}

export interface ListItem {
  depth: number;
  text: Inline[];
  /** A task box: `- [ ]` is false, `- [x]` true. */
  done?: boolean;
}

export type Block =
  | { t: "h"; level: number; text: Inline[] }
  | { t: "p"; text: Inline[] }
  | { t: "code"; s: string }
  | { t: "list"; ordered: boolean; items: ListItem[] }
  | { t: "quote"; text: Inline[] }
  | { t: "hr" }
  | { t: "table"; head: Inline[][]; rows: Inline[][][] };

const INLINE = /`([^`]+)`|\*\*(.+?)\*\*|(?<![\w*])\*(?!\s)(.+?)(?<!\s)\*(?![\w*])|(?<!\w)_(?!\s)(.+?)(?<!\s)_(?!\w)|\[([^\]]+)\]\(([^)\s]+)\)/g;

export function parseInline(s: string): Inline[] {
  const out: Inline[] = [];
  let at = 0;
  for (const m of s.matchAll(INLINE)) {
    if (m.index > at) out.push({ t: "text", s: s.slice(at, m.index) });
    if (m[1] !== undefined) out.push({ t: "code", s: m[1] });
    else if (m[2] !== undefined) out.push({ t: "strong", s: m[2] });
    else if (m[3] !== undefined) out.push({ t: "em", s: m[3] });
    else if (m[4] !== undefined) out.push({ t: "em", s: m[4] });
    else out.push({ t: "link", s: m[5], href: m[6] });
    at = m.index + m[0].length;
  }
  if (at < s.length) out.push({ t: "text", s: s.slice(at) });
  return out;
}

const HEADING = /^(#{1,6})\s+(.*?)\s*#*\s*$/;
const ITEM = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const TASK = /^\[([ xX])\]\s+(.*)$/;
const RULE = /^\s*([-*_])(\s*\1){2,}\s*$/;
const TABLE_SEP = /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/;

function cells(row: string): Inline[][] {
  const inner = row.trim().replace(/^\|/, "").replace(/\|$/, "");
  return inner.split("|").map((c) => parseInline(c.trim()));
}

export function parseMarkdown(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  const blocks: Block[] = [];
  let i = 0;
  const startsBlock = (l: string, next: string | undefined) =>
    HEADING.test(l) || ITEM.test(l) || RULE.test(l) || l.startsWith(">") || l.trimStart().startsWith("```") || (l.includes("|") && next !== undefined && TABLE_SEP.test(next));
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    if (line.trimStart().startsWith("```")) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trimStart().startsWith("```")) body.push(lines[i++]);
      i++; // the closing fence, if there is one
      blocks.push({ t: "code", s: body.join("\n") });
      continue;
    }
    const h = HEADING.exec(line);
    if (h) {
      blocks.push({ t: "h", level: h[1].length, text: parseInline(h[2]) });
      i++;
      continue;
    }
    if (RULE.test(line)) {
      blocks.push({ t: "hr" });
      i++;
      continue;
    }
    if (line.includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1])) {
      const head = cells(line);
      const rows: Inline[][][] = [];
      i += 2;
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) rows.push(cells(lines[i++]));
      blocks.push({ t: "table", head, rows });
      continue;
    }
    if (line.startsWith(">")) {
      const text: string[] = [];
      while (i < lines.length && lines[i].startsWith(">")) text.push(lines[i++].replace(/^>\s?/, ""));
      blocks.push({ t: "quote", text: parseInline(text.join(" ")) });
      continue;
    }
    const first = ITEM.exec(line);
    if (first) {
      const ordered = /\d/.test(first[2]);
      const base = first[1].length;
      const items: ListItem[] = [];
      while (i < lines.length) {
        const m = ITEM.exec(lines[i]);
        // A top-level item of the other kind starts a new list.
        if (m && m[1].length <= base && /\d/.test(m[2]) !== ordered) break;
        if (m) {
          const task = TASK.exec(m[3]);
          const depth = Math.max(0, Math.floor((m[1].length - base) / 2));
          items.push({ depth, text: parseInline(task ? task[2] : m[3]), ...(task ? { done: task[1] !== " " } : {}) });
          i++;
        } else if (lines[i].trim() && /^\s+/.test(lines[i]) && items.length) {
          // A wrapped item continues on an indented line.
          items[items.length - 1].text.push({ t: "text", s: " " }, ...parseInline(lines[i].trim()));
          i++;
        } else break;
      }
      blocks.push({ t: "list", ordered, items });
      continue;
    }
    const text: string[] = [line.trim()];
    i++;
    while (i < lines.length && lines[i].trim() && !startsBlock(lines[i], lines[i + 1])) text.push(lines[i++].trim());
    blocks.push({ t: "p", text: parseInline(text.join(" ")) });
  }
  return blocks;
}
