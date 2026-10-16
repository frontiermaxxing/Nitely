// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { issueSpendLine, projectSpendWords, shortTokens } from "./spend.ts";
import type { IssueDetail, IssueRecord, IssueSpend, ProjectSpend } from "./types.ts";

const issue = { key: "X-1" } as IssueRecord;
const detail = (timeline: IssueDetail["timeline"], reviews = 0): IssueDetail =>
  ({ issue, relations: { blocked_by: [], blocks: [] }, timeline, prs: [], reviews: Array.from({ length: reviews }, () => ({})) }) as unknown as IssueDetail;
const spend = (tokens: number, by: [string, number][]): IssueSpend => ({
  key: "X-1",
  tokens,
  cache_read: 0,
  calls: 12,
  sessions: 1,
  by_model: by.map(([model, t]) => ({ vendor: "claude", model, tokens: t, calls: 6 })),
  first: null,
  last: null,
});

test("token counts read short", () => {
  assert.deepEqual([shortTokens(512), shortTokens(86_400), shortTokens(1_234_567), shortTokens(12_600_000)], ["512", "86k", "1.2M", "13M"]);
});

test("an issue's spend is one line of what is known", () => {
  const t = [
    { ts: 1000, actor: "a", kind: "worktree", summary: "Started" },
    { ts: 1000 + 3 * 3600 + 600, actor: "a", kind: "landed", summary: "Landed" },
  ];
  assert.equal(
    issueSpendLine(detail(t, 2), spend(412_000, [["claude-opus-5-5", 380_000], ["gpt-6", 32_000]]), 0),
    "3h 10m from start to land · 412k tokens (opus-5-5 380k, gpt-6 32k) · 12 model calls · 2 review rounds",
  );
  assert.equal(issueSpendLine(detail(t.slice(0, 1)), spend(0, []), 1000 + 1800), "30m so far", "no tokens seen: left out");
  assert.equal(issueSpendLine(detail([]), null, 0), null);
});

test("a project's spend says its total and, on hover, where it went", () => {
  const s: ProjectSpend = {
    project: "X",
    days: [{ day_start: 0, tokens: 1_500_000, landed: 2 }],
    tokens: 1_500_000,
    cache_read: 90_000_000,
    by_agent: [{ vendor: "claude", model: "", tokens: 1_500_000, calls: 300 }],
    by_model: [{ vendor: "claude", model: "claude-opus-5-5", tokens: 1_500_000, calls: 300 }],
    landed: 2,
    per_landed: 600_000,
    top_issues: [{ key: "X-3", tokens: 900_000 }],
  };
  const w = projectSpendWords(s);
  assert.equal(w.total, "1.5M tokens");
  assert.match(w.title, /2 landed, 600k tokens per landed issue/);
  assert.match(w.title, /X-3: 900k/);
});
