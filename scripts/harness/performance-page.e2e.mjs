// Cached Performance page, recorder persistence and human controls on isolated harness data.
// CI only by default; app.mjs owns the native harness guard and one-window lock.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { launch } from "./app.mjs";
const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
const sleep = ms => new Promise(r => setTimeout(r, ms));

test("Performance history survives an app restart and only clears through paused human controls", { timeout: 180000 }, async () => {
  const app = await launch();
  const query = () => app.page.invoke("performance_query", { query: { view: "history", limit: 100 } });
  try {
    await app.page.open("#performance");
    await app.page.waitFor(`return document.querySelector('[data-testid="performance-page"]') !== null`, { what: "Performance route" });
    let recorded;
    for (let i = 0; i < 40; i++) { recorded = await query(); if (recorded.latest) break; await sleep(500); }
    assert.ok(recorded.latest, "the isolated recorder produced a sample");
    assert.ok(recorded.points.length, "history reads the saved sample");
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('Recording')`, { what: "recording freshness" });
    await app.page.screenshot(join(SCREENS, "performance-recording.png"));
    assert.equal(await app.page.eval(`return [...document.querySelectorAll('button')].find(b => b.textContent === 'Clear history…')?.disabled`), true);
    await app.page.click({ css: "button", text: "Pause collection" });
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('paused')`, { what: "paused collection" });
    assert.equal((await query()).settings.enabled, false);
    await app.restart();
    await app.page.open("#performance");
    const restored = await query();
    assert.equal(restored.settings.enabled, false, "pause is durable");
    assert.ok(restored.points.some(p => p.to_ms === recorded.latest.wall_ms), "recorded history survives app/recorder restart");
    // Restarting the app is not evidence of a machine reboot.
    await assert.rejects(app.page.invoke("performance_query", { query: { view: "history", before_last_restart: true } }), /boot transition/);
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('paused')`, { what: "restored paused state" });
    await app.page.click({ css: "button", text: "Clear history…" });
    await app.page.click({ css: "button", text: "Cancel" });
    assert.ok((await query()).latest, "cancelling preserves history");
    await app.page.click({ css: "button", text: "Clear history…" });
    await app.page.click({ css: "button", text: "Clear recorded history" });
    await app.page.waitFor(`return document.body.textContent.includes('Recorded history cleared.')`, { what: "clear completion" });
    const cleared = await query(); assert.equal(cleared.latest, null); assert.equal(cleared.points.length, 0);
    await app.page.screenshot(join(SCREENS, "performance-paused-empty.png"));
    await app.page.click({ css: "button", text: "Resume collection" });
    await app.page.waitFor(`return document.body.textContent.includes('Collection enabled. Settings saved.')`, { what: "resume completion" });
    assert.equal((await query()).settings.enabled, true);
  } finally { await app.close(); }
});
