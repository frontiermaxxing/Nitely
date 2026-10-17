// The harness app never sees the person's own agents (NIT-157): Agent setup works on the run's
// agents' home inside its data folder, not on the real ~/.claude, ~/.codex or ~/.cursor.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { launch, nitely, seedSample } from "./app.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
const norm = (p) => resolve(p).replace(/\\/g, "/").toLowerCase();

test("Agent setup works on the run's own agents' home, never the person's", { timeout: 180000 }, async () => {
  // A watched repo (Agent setup plans nothing without one) and Claude Code "installed" in the
  // throwaway agents' home, so Agent setup has something to plan for.
  const app = await launch({
    seed: (home) => {
      seedSample(home);
      const repo = join(home, "repo");
      mkdirSync(repo, { recursive: true });
      spawnSync("git", ["init", "-q", "-b", "main", repo], { stdio: "ignore" });
      nitely(home, "repo", "add", repo, "--integration", "main", "--project", "APP");
      mkdirSync(join(home, "agents", ".claude"), { recursive: true });
    },
  });
  const { page, home } = app;
  try {
    const agents = norm(join(home, "agents"));
    const plans = await page.invoke("agent_setup");
    const claude = plans.find((p) => p.agent === "Claude Code");
    assert.ok(claude?.found, `Claude Code is found in the run's agents' home: ${JSON.stringify(plans.map((p) => [p.agent, p.found]))}`);
    const paths = plans.flatMap((p) => p.changes ?? []).filter((c) => c.action !== "run").map((c) => c.target);
    assert.ok(paths.length > 0, "Agent setup plans files to write");
    for (const target of paths) assert.ok(norm(target).startsWith(`${agents}/`), `${target} is inside the run's agents' home ${agents}`);

    await page.open("#agents");
    await page.waitFor(`return document.querySelector(".vendor-card") !== null;`, { what: "Agent setup's cards" });
    const text = await page.eval(`return document.body.innerText;`);
    for (const dir of [".claude", ".codex", ".cursor"]) {
      assert.ok(!text.includes(join(homedir(), dir)), `the page names no ${dir} in the person's home`);
    }
    await page.screenshot(join(SCREENS, "agents-isolated.png"));
  } catch (e) {
    console.error(app.log());
    throw e;
  } finally {
    await app.close();
  }
});
