// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { canContinue, openingStep, opensOnLaunch, stepStates, stepSummary, toolsReady, type SetupFacts } from "./steps.ts";
import type { AgentPlan, Tool } from "../types.ts";

const tool = (command: string, name: string, path: string | null, signed_in: boolean | null = null, version: string | null = null): Tool => ({ command, name, path, version, signed_in });
const nothing = [tool("git", "Git", null), tool("claude", "Claude Code", null), tool("codex", "Codex", null), tool("gh", "GitHub CLI", null)];
const ready = [tool("git", "Git", "/usr/bin/git", null, "2.47.1"), tool("claude", "Claude Code", "/x/claude", true, "2.1.3"), tool("codex", "Codex", "/x/codex", false, "0.48.0"), tool("gh", "GitHub CLI", null)];
const agent = (name: string, found: boolean, pending: number): AgentPlan => ({
  agent: name,
  found,
  changes: Array.from({ length: pending }, (_, i) => ({ target: `~/${name}/${i}`, action: "update" as const, why: "", after: "" })),
});
const facts = (over: Partial<SetupFacts> = {}): SetupFacts => ({ tools: nothing, projects: [], agents: [], seen: [], ...over });

test("NIT-690: a step is done when what it sets up reports ready, and needs the person when something says no", () => {
  // Nothing found: Tools needs the person; nothing else is done yet.
  assert.deepEqual(stepStates(facts()), { welcome: "todo", tools: "needs", signin: "todo", project: "todo", agents: "todo", skills: "todo" });
  // git and one agent make Tools done; an installed agent that says it is signed out needs the person.
  const partly = stepStates(facts({ tools: ready, seen: ["welcome"] }));
  assert.equal(partly.tools, "done");
  assert.equal(partly.signin, "needs");
  // Everything set up: every step done.
  const allDone = facts({
    tools: ready.map((t) => (t.command === "codex" ? { ...t, signed_in: true } : t)),
    projects: [{ key: "APP", name: "my-app" }],
    agents: [agent("Claude Code", true, 0), agent("Cursor", false, 2)],
    seen: ["welcome", "skills"],
  });
  assert.deepEqual(Object.values(stepStates(allDone)), ["done", "done", "done", "done", "done", "done"]);
  // A tool that can't say whether it is signed in does not hold the step as needing the person.
  assert.equal(stepStates(facts({ tools: ready.map((t) => ({ ...t, signed_in: t.command === "claude" ? null : t.signed_in === false ? null : t.signed_in })) })).signin, "todo");
  // An agent with changes still to make leaves Agents not done.
  assert.equal(stepStates(facts({ agents: [agent("Codex", true, 1)] })).agents, "todo");
  // A broken config, or a change the person makes by hand, needs the person: never done (NIT-693 review).
  assert.equal(stepStates(facts({ agents: [{ ...agent("Codex", true, 0), error: "config.toml is not valid TOML" }] })).agents, "needs");
  assert.equal(stepStates(facts({ agents: [{ ...agent("Codex", true, 0), changes: [{ target: "~/.codex", action: "manual" as const, why: "", after: "" }] }] })).agents, "needs");
});

test("NIT-690: reopened from Settings it opens on the first step not done; on launch, where it was left", () => {
  const f = facts({ tools: ready, seen: ["welcome"] });
  assert.equal(openingStep(f, true, null), "signin", "Codex is signed out");
  assert.equal(openingStep({ ...f, seen: [] }, true, null), "signin", "reopened, Welcome never holds it");
  assert.equal(openingStep(f, false, "project"), "project");
  assert.equal(openingStep(f, false, null), "welcome");
});

test("NIT-690: Continue waits for git and one agent, and for a project; the app opens it only when something is missing", () => {
  assert.equal(toolsReady(nothing), false);
  assert.equal(canContinue("tools", facts()), false);
  assert.equal(canContinue("tools", facts({ tools: ready })), true);
  assert.equal(canContinue("project", facts({ tools: ready })), false);
  assert.equal(canContinue("signin", facts({ tools: ready })), true, "signing in can wait");
  assert.equal(opensOnLaunch(null, 0, false), false, "not before the tools were checked");
  assert.equal(opensOnLaunch(ready, 0, false), true, "no project yet");
  assert.equal(opensOnLaunch(nothing, 3, false), true, "a needed tool is missing");
  assert.equal(opensOnLaunch(ready, 1, false), false);
  assert.equal(opensOnLaunch(nothing, 0, true), false, "closed by the person");
});

test("NIT-690: a done step's line says what it found", () => {
  const f = facts({ tools: ready, projects: [{ key: "APP", name: "my-app" }], agents: [agent("Claude Code", true, 0), agent("Codex", true, 0)] });
  assert.equal(stepSummary("tools", f), "git 2.47 · Claude Code 2.1 · Codex 0.48");
  assert.equal(stepSummary("signin", f), "Codex isn't signed in");
  assert.equal(stepSummary("project", f), "my-app · APP");
  assert.equal(stepSummary("agents", f), "Claude Code and Codex");
  assert.equal(stepSummary("agents", { ...f, agents: [agent("Claude Code", true, 0), agent("Codex", true, 0), agent("Cursor", true, 0)] }), "Claude Code, Codex and Cursor");
});
