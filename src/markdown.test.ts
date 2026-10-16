// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { parseInline, parseMarkdown } from "./markdown.ts";

test("inline code, bold, italics and links", () => {
  assert.deepEqual(parseInline("run `npm ci`, **then** *gate* _now_ [NIT-1](nitely://issue/NIT-1)."), [
    { t: "text", s: "run " },
    { t: "code", s: "npm ci" },
    { t: "text", s: ", " },
    { t: "strong", s: "then" },
    { t: "text", s: " " },
    { t: "em", s: "gate" },
    { t: "text", s: " " },
    { t: "em", s: "now" },
    { t: "text", s: " " },
    { t: "link", s: "NIT-1", href: "nitely://issue/NIT-1" },
    { t: "text", s: "." },
  ]);
});

test("snake_case and arithmetic stars stay text", () => {
  assert.deepEqual(parseInline("setup_completed_at and 2 * 3 * 4"), [{ t: "text", s: "setup_completed_at and 2 * 3 * 4" }]);
});

test("blocks: headings, paragraphs, lists, tasks, quotes, rules, code", () => {
  const md = [
    "# Brief",
    "",
    "The vision, as",
    "read from the code.",
    "- one",
    "  - nested that",
    "    wraps",
    "- [x] done",
    "- [ ] open",
    "1. first",
    "> inferred",
    "---",
    "```",
    "cargo test",
    "```",
  ].join("\n");
  const b = parseMarkdown(md);
  assert.deepEqual(
    b.map((x) => x.t),
    ["h", "p", "list", "list", "quote", "hr", "code"],
  );
  assert.deepEqual(b[1], { t: "p", text: [{ t: "text", s: "The vision, as read from the code." }] });
  const list = b[2];
  assert.ok(list.t === "list" && !list.ordered);
  assert.deepEqual(
    list.items.map((i) => [i.depth, i.done]),
    [
      [0, undefined],
      [1, undefined],
      [0, true],
      [0, false],
    ],
  );
  assert.deepEqual(list.items[1].text, [{ t: "text", s: "nested that" }, { t: "text", s: " " }, { t: "text", s: "wraps" }]);
  assert.ok(b[3].t === "list" && b[3].ordered);
  assert.deepEqual(b[6], { t: "code", s: "cargo test" });
});

test("pipe tables", () => {
  const b = parseMarkdown("| Area | State |\n|---|:--|\n| core | `ok` |\n| ui | rough |\n\nafter");
  assert.equal(b.length, 2);
  const t = b[0];
  assert.ok(t.t === "table");
  assert.deepEqual(t.head, [[{ t: "text", s: "Area" }], [{ t: "text", s: "State" }]]);
  assert.deepEqual(t.rows[0][1], [{ t: "code", s: "ok" }]);
  assert.equal(t.rows.length, 2);
});

test("an unclosed fence keeps the rest as code; CRLF is fine", () => {
  assert.deepEqual(parseMarkdown("```\r\na\r\nb"), [{ t: "code", s: "a\nb" }]);
});
