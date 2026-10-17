// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Decision } from "./decisions.ts";
import { DEFAULT_SETTINGS, FORGET_SECS, freshDecisions, inQuiet, inView, kindOf, messageOf, type NotifySettings } from "./notify.ts";
import type { Ask, IssueDetail, IssueRecord } from "./types.ts";

const NOW = 1_800_000_000;

const issue = (key: string, state = "Needs You"): IssueRecord =>
  ({ key, origin: "nitely", project: "X", title: `Title ${key}`, state, state_type: "started", priority: 3, urgency: 3, owner: "agent" }) as unknown as IssueRecord;

const ask = (id: number, key: string): Ask => ({ id, issue_key: key, asked_by: "claude:c:/x", question: "Keep the old flag?", options: [], answer: null, asked_at: NOW - 5, answered_at: null });

const decision = (kind: Decision["kind"], key: string, extra: Partial<Decision> = {}): Decision => ({
  id: `${kind}-${key}`,
  kind,
  issue: issue(key),
  row: null,
  repo: null,
  ask: null,
  since: NOW - 60,
  urgency: 3,
  ...extra,
});

const detail = (key: string, summary: string): IssueDetail =>
  ({ issue: issue(key), relations: { blocked_by: [], blocks: [] }, timeline: [{ ts: NOW - 60, actor: "crew", kind: "issue_moved", summary }], prs: [], reviews: [] }) as unknown as IssueDetail;

const all = (ds: Decision[]) => new Map(ds.filter((d) => d.issue).map((d) => [d.issue!.key, detail(d.issue!.key, "Todo → Needs You")]));
const run = (ds: Decision[], seen: Record<string, number> | null, settings: NotifySettings = DEFAULT_SETTINGS, inboxInView = false, details = all(ds)) =>
  freshDecisions(ds, details, seen, settings, NOW, { inboxInView, at: new Date(2026, 9, 16, 15, 0) });

test("the first run takes what already waits as seen, and notifies nothing", () => {
  const r = run([decision("approve", "X-1")], null);
  assert.equal(r.fresh.length, 0);
  assert.deepEqual(Object.keys(r.seen), ["waits-X-1"]);
});

test("a new decision notifies once, never twice", () => {
  const a = decision("ask", "X-2", { id: "ask-7", ask: ask(7, "X-2") });
  const first = run([a], {});
  assert.equal(first.fresh.length, 1);
  assert.equal(first.fresh[0].line, "X-2 asks: Keep the old flag?");
  assert.equal(run([a], first.seen).fresh.length, 0);
});

test("a decision that briefly drops off a stale board does not notify again; one gone a while does", () => {
  const a = decision("approve", "X-3");
  const gone = { "waits-X-3": NOW - 60 };
  assert.equal(run([a], gone).fresh.length, 0);
  assert.equal(run([a], { "waits-X-3": NOW - FORGET_SECS - 1 }).fresh.length, 1);
});

test("nothing notifies while the Inbox is in view, in quiet hours, or for a kind turned off; all count as seen", () => {
  const a = decision("approve", "X-4");
  const viewed = run([a], {}, DEFAULT_SETTINGS, true);
  assert.equal(viewed.fresh.length, 0);
  assert.ok("waits-X-4" in viewed.seen);
  const quiet: NotifySettings = { ...DEFAULT_SETTINGS, quiet: { on: true, from: "14:00", to: "16:00" } };
  assert.equal(run([a], {}, quiet).fresh.length, 0);
  const off: NotifySettings = { ...DEFAULT_SETTINGS, kinds: { ...DEFAULT_SETTINGS.kinds, approval: false } };
  assert.equal(run([a], {}, off).fresh.length, 0);
});

test("quiet hours can run past midnight", () => {
  const s: NotifySettings = { ...DEFAULT_SETTINGS, quiet: { on: true, from: "22:00", to: "08:00" } };
  assert.equal(inQuiet(s, new Date(2026, 9, 16, 23, 30)), true);
  assert.equal(inQuiet(s, new Date(2026, 9, 16, 7, 59)), true);
  assert.equal(inQuiet(s, new Date(2026, 9, 16, 8, 0)), false);
  assert.equal(inQuiet({ ...s, quiet: { ...s.quiet, on: false } }, new Date(2026, 9, 16, 23, 30)), false);
});

test("an issue the crew gave up on notifies as the crew's, and the person's own work never notifies", () => {
  const gaveUp = new Map([["X-5", detail("X-5", "In Progress → Needs You: the crew tried it 3 times")]]);
  assert.equal(kindOf(decision("approve", "X-5"), gaveUp.get("X-5")), "crew");
  assert.equal(kindOf(decision("approve", "X-6")), "approval");
  assert.equal(kindOf(decision("conflict", "X-7")), "land");
  assert.equal(kindOf(decision("own", "X-8")), null);
  const r = run([decision("approve", "X-5"), decision("own", "X-8")], {}, DEFAULT_SETTINGS, false, gaveUp);
  assert.deepEqual(r.fresh.map((f) => f.line), ["The crew gave up on X-5: Title X-5"]);
});

test("a batch becomes one notification that opens the Inbox; one decision opens itself", () => {
  const fresh = run([decision("approve", "X-9"), decision("land", "X-10"), decision("approve", "X-11")], {}).fresh;
  const many = messageOf(fresh);
  assert.equal(many.title, "3 decisions waiting");
  assert.equal(many.decision, null);
  assert.match(many.body, /and 1 more$/);
  const one = messageOf(fresh.slice(0, 1));
  assert.equal(one.decision, "approve-X-9");
  assert.equal(one.title, "Waiting for your approval");
});

test("an approval waits unseen until its detail says whether the crew gave up", () => {
  const a = decision("approve", "X-12");
  const r = freshDecisions([a], new Map(), {}, DEFAULT_SETTINGS, NOW, { inboxInView: false, at: new Date(2026, 9, 16, 15, 0) });
  assert.equal(r.fresh.length, 0);
  assert.equal("waits-X-12" in r.seen, false);
  const later = freshDecisions([a], new Map([["X-12", detail("X-12", "In Progress → Needs You: the crew tried it 3 times")]]), r.seen, DEFAULT_SETTINGS, NOW, { inboxInView: false, at: new Date(2026, 9, 16, 15, 0) });
  assert.equal(later.fresh[0].kind, "crew");
});

test("an issue that turns from the person's own work into an approval once its detail is read is not new", () => {
  const first = run([decision("own", "X-13")], {});
  assert.equal(first.fresh.length, 0);
  assert.equal(run([decision("approve", "X-13")], first.seen).fresh.length, 0);
});

test("a playbook change waiting for approval is told once its detail is read, not swallowed as the person's own work", () => {
  const own = decision("own", "X-14");
  const before = freshDecisions([own], new Map(), {}, DEFAULT_SETTINGS, NOW, { inboxInView: false, at: new Date(2026, 9, 16, 15, 0) });
  assert.equal("waits-X-14" in before.seen, false);
  const after = run([decision("approve", "X-14")], before.seen);
  assert.equal(after.fresh.length, 1);
});

test("a decision that keeps waiting stays seen as long as it waits", () => {
  const a = decision("approve", "X-15");
  let seen: Record<string, number> = { "waits-X-15": NOW };
  for (let t = 1; t <= 30; t++) {
    const r = freshDecisions([a], all([a]), seen, DEFAULT_SETTINGS, NOW + t * 60, { inboxInView: false, at: new Date(2026, 9, 16, 15, 0) });
    assert.equal(r.fresh.length, 0);
    seen = r.seen;
  }
});

test("NIT-236: with the Inbox filtered to a project, another project's new decisions still notify", () => {
  const mine = decision("approve", "APP-1", { project: "APP" });
  const theirs = decision("approve", "WEB-1", { project: "WEB" });
  const at = { at: new Date(2026, 9, 16, 15, 0) };
  const r = freshDecisions([mine, theirs], all([mine, theirs]), {}, DEFAULT_SETTINGS, NOW, { inboxInView: true, inboxProject: "app", ...at });
  assert.deepEqual(r.fresh.map((f) => f.decision.id), ["approve-WEB-1"]);
  assert.equal(freshDecisions([theirs], all([theirs]), {}, DEFAULT_SETTINGS, NOW, { inboxInView: true, ...at }).fresh.length, 0, "unfiltered, the Inbox in view lists it");
  assert.deepEqual([inView(mine, { inboxInView: true, inboxProject: "APP" }), inView(theirs, { inboxInView: true, inboxProject: "APP" }), inView(mine, { inboxInView: false })], [true, false, false]);
});
