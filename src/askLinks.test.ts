import { test } from "node:test";
import assert from "node:assert/strict";
import { askOfEntry } from "./askLinks.ts";
import { askIdOf } from "./askToken.ts";
import type { Ask } from "./types.ts";

const ask = (id: number, question: string, asked_at: number, answered_at: number | null): Ask => ({
  id, issue_key: "APP-3", asked_by: "claude:c:/code/app", question, options: [], answer: answered_at ? "Yes" : null, asked_at, answered_at,
});

test("an ask or answer entry on the timeline links its ask, and nothing else does (NIT-546)", () => {
  const asks = [ask(68, "Keep the flag?", 100, 300), ask(69, "Ship it?", 200, null)];
  assert.equal(askOfEntry({ kind: "ask", ts: 200, summary: "Ship it?" }, asks), 69);
  assert.equal(askOfEntry({ kind: "ask", ts: 100, summary: "Keep the flag? " }, asks), 68);
  assert.equal(askOfEntry({ kind: "answer", ts: 300, summary: "Yes" }, asks), 68);
  assert.equal(askOfEntry({ kind: "ask", ts: 200, summary: "Something else" }, asks), null, "same second, another question");
  assert.equal(askOfEntry({ kind: "note", ts: 200, summary: "Ship it?" }, asks), null);
  assert.equal(askOfEntry({ kind: "ask", ts: 200, summary: "Ship it?" }, undefined), null, "an older backend sends no asks");
});

test("an ask:<n> token opens that ask, whatever its case, and nothing else does", () => {
  assert.equal(askIdOf("ask:69"), 69);
  assert.equal(askIdOf("ASK:69"), 69, "the hash upper-cases what follows @");
  assert.equal(askIdOf("NIT-69"), null);
  assert.equal(askIdOf("ask:"), null);
  assert.equal(askIdOf(null), null);
});
