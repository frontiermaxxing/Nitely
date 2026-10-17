// End to end in the real app: the Inbox on seeded data, an approval and an answer, each resolved
// in place and written to the database. `npm run app:test` (after `npm run app:build`); CI runs it
// on Windows. Screenshots go to target/harness/screens.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { badge, launch, nitely } from "./app.mjs";
import { gpuInfo } from "./cdp.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });

test("the Inbox approves an issue and answers a question, and the database shows both", { timeout: 180000 }, async () => {
  const app = await launch();
  const { page, home } = app;
  try {
    // The app draws without the GPU (tauri.harness.conf.json, NIT-188): its browser says GPU
    // compositing and rasterization are off. (A WebGL renderer string cannot tell: on a machine
    // with no graphics card, as in CI, it names Microsoft's software driver either way.)
    const gpu = await gpuInfo(app.port);
    console.log(`GPU features: ${JSON.stringify(gpu.featureStatus)}`);
    assert.match(String(gpu.featureStatus.gpu_compositing), /^disabled/, "GPU compositing is off in the harness app");
    assert.match(String(gpu.featureStatus.rasterization), /^disabled/, "GPU rasterization is off in the harness app");

    await page.open("#inbox");
    await page.waitFor(`return document.querySelectorAll(".inbox-row").length >= 2;`, { what: "two decisions in the Inbox" });
    await page.screenshot(join(SCREENS, "inbox-seeded.png"));

    // The approval: pick it, approve, and see it resolve in place.
    await page.click({ css: ".inbox-row", text: "Ship the export button" });
    await page.click({ css: ".inbox-detail button", text: "Approve" });
    const outcome = await page.waitFor(`return document.querySelector(".dd-outcome.done")?.textContent;`, { what: "the approval's outcome" });
    assert.match(outcome, /Approved/);
    await page.screenshot(join(SCREENS, "inbox-approved.png"));
    const approved = JSON.parse(nitely(home, "issue", "show", "APP-1"));
    assert.equal(approved.issue.state, "Approved");

    // The question: answer with its first option.
    await page.click({ css: ".inbox-row", text: "Should the default theme follow the system?" });
    await page.click({ css: ".dd-option", text: "Follow the system" });
    const answered = await page.waitFor(`return document.querySelector(".dd-outcome.done")?.textContent;`, { what: "the answer's outcome" });
    assert.match(answered, /Answered/);
    const shown = nitely(home, "issue", "show", "APP-2");
    assert.match(shown, /Follow the system/);

    // Both decisions leave the Inbox once their moment has passed.
    await page.waitFor(`return document.querySelectorAll(".inbox-row").length === 0 || !!document.querySelector(".empty-state, .ui-empty");`, { timeoutMs: 20000, what: "the Inbox to empty" });
    await page.screenshot(join(SCREENS, "inbox-empty.png"));

    // The taskbar count clears with it: no number, no overlay drawn.
    for (let i = 0; i < 40 && badge(home)?.count !== 0; i++) await new Promise((r) => setTimeout(r, 250));
    assert.deepEqual(badge(home), { count: 0, drawn: false }, "the taskbar count clears at zero");
  } catch (e) {
    console.error(app.log());
    throw e;
  } finally {
    await app.close();
  }
});
