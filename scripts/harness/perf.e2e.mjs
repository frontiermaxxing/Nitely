// The real app on the large board (crates/nitely-core/src/perf.rs, seeded by `nitely perf-seed`:
// 4 projects, 800 issues, 30 worktrees, 5,000 events): start-up, the Inbox and an issue opening, and
// the page's memory over many live updates, each within its budget (NIT-255). `npm run app:test`;
// CI runs it on Windows. The harness app is a debug Rust build, so its times are slower than a
// release's; the budgets allow for that and for CI's runners. The backend's own reads have release
// budgets in perf.rs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { launch, nitely } from "./app.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });

// From CI's Windows runner, the harness's debug build (2026-10-18): start-up 3.2 s, the Inbox with
// 100 decisions 110 ms, an issue panel 223 ms, the heap 4.4 to 4.7 MB over 240 refreshes (about 2 MB
// an hour). The budgets give a slower run about two and a half times that.
const BUDGET = {
  /** Process started to the first board drawn. */
  startMs: 8000,
  /** The Inbox opened to its decisions drawn. */
  inboxMs: 1500,
  /** An issue's panel opened to its details drawn. */
  issueMs: 1500,
  /** The page's live JS heap, after a garbage collection, growing over an hour of live updates. */
  heapGrowthPerHourMb: 20,
};

/** Refreshes in an hour of a busy board: the app refreshes at most once a second, and agents at
 * work change something every few seconds. */
const REFRESHES_PER_HOUR = 1200;
const REFRESHES = 240;

test("the app stays within its budgets on a large board", { timeout: 900000 }, async () => {
  const app = await launch({ seed: (home) => console.log(nitely(home, "perf-seed").trim()) });
  const { page } = app;
  const check = (what, value, budget, unit) => {
    // Said as it is measured, so a run that fails later still shows what came before.
    console.log(`${what.padEnd(34)} ${String(Math.round(value)).padStart(7)} ${unit}  (budget ${budget} ${unit})`);
    return value <= budget ? null : `${what}: ${Math.round(value)} ${unit}, budget ${budget} ${unit}`;
  };
  const over = [];
  try {
    // Start-up: the process start to the first board drawn, both on the wall clock.
    const firstBoard = await page.waitFor(
      `const m = performance.getEntriesByName("nitely:first-board")[0]; return m ? performance.timeOrigin + m.startTime : null;`,
      { timeoutMs: 60000, what: "the first board" },
    );
    over.push(check("start-up", firstBoard - app.startedAt, BUDGET.startMs, "ms"));
    await page.screenshot(join(SCREENS, "perf-start.png"));

    // The Inbox: opened to its decisions drawn.
    let t = Date.now();
    await page.open("#inbox");
    const rows = await page.waitFor(`const n = document.querySelectorAll(".inbox-row").length; return n >= 10 ? n : null;`, { timeoutMs: 60000, what: "the Inbox's decisions" });
    over.push(check(`inbox (${rows} decisions)`, Date.now() - t, BUDGET.inboxMs, "ms"));
    await page.screenshot(join(SCREENS, "perf-inbox.png"));

    // An issue: its panel opened to its details drawn.
    const issues = await page.invoke("issues", { project: "APP", includeClosed: false });
    const key = issues.find((i) => i.state === "In Progress")?.key;
    assert.ok(key, `an issue In Progress in APP: ${issues.length} open`);
    t = Date.now();
    await page.open(`#now@${key}`);
    await page.waitFor(`return document.querySelector("aside.panel .panel-title") !== null;`, { timeoutMs: 30000, what: `${key}'s panel` });
    over.push(check("issue panel", Date.now() - t, BUDGET.issueMs, "ms"));
    await page.open("#inbox");

    // Memory: the board changed and drawn again many times, as live updates do, on the Inbox (the
    // view that reads the most). Growth is measured from after a warm-up, each time after a
    // garbage collection, and projected to an hour.
    const heap = async () => {
      await page.send("HeapProfiler.collectGarbage");
      return (await page.send("Runtime.getHeapUsage")).usedSize / 1e6;
    };
    // In batches, so no one call to the page runs past the driver's timeout.
    const refresh = async (count) => {
      let nodes = 0;
      for (let done = 0; done < count; done += 20) {
        nodes = await page.eval(`
          for (let i = 0; i < ${Math.min(20, count - done)}; i++) {
            const drawn = new Promise((r) => window.addEventListener("nitely:board-drawn", r, { once: true }));
            await window.__TAURI_INTERNALS__.invoke("plugin:event|emit", { event: "nitely://changed", payload: null });
            await drawn;
          }
          return document.getElementsByTagName("*").length;`);
      }
      return nodes;
    };
    await refresh(20);
    const before = await heap();
    t = Date.now();
    const nodes = await refresh(REFRESHES);
    const seconds = (Date.now() - t) / 1000;
    const after = await heap();
    const perHour = ((after - before) / REFRESHES) * REFRESHES_PER_HOUR;
    console.log(`heap ${before.toFixed(1)} MB -> ${after.toFixed(1)} MB over ${REFRESHES} refreshes (${(seconds / REFRESHES * 1000).toFixed(0)} ms each), ${nodes} DOM nodes`);
    over.push(check("heap growth per hour", perHour, BUDGET.heapGrowthPerHourMb, "MB"));
    await page.screenshot(join(SCREENS, "perf-after-refreshes.png"));

    const broken = over.filter(Boolean);
    assert.deepEqual(broken, [], `over budget:\n${broken.join("\n")}`);
  } finally {
    await app.close();
  }
});
