// The performance recorder across the app's own restart, checked through its state only (NIT-548):
// a quit saves a clean stop, history survives, a pause is durable, and history clears only while
// paused.
import { test } from "node:test";
import assert from "node:assert/strict";
import { launch, nitely } from "./app.mjs";

/** A native command's rejection as text, or null when it succeeded. */
const rejection = (page, command, args) =>
  page.eval(`
    try {
      await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)});
      return null;
    } catch (error) { return typeof error === "string" ? error : error instanceof Error ? error.message : JSON.stringify(error); }`);

test("the recorder keeps its history, a clean stop and a pause across a restart, and clears only while paused", { timeout: 180000 }, async () => {
  const app = await launch();
  const status = () => JSON.parse(nitely(app.home, "performance", "status"));
  const query = () => app.page.invoke("performance_query", { query: { view: "history", limit: 100 } });
  try {
    let before;
    const deadline = Date.now() + 15000;
    while (!(before = status()).latest && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 100));
    assert.ok(before.latest, "desktop collected a durable sample");
    assert.equal(before.session.stopped_ms, null, "desktop owns the active session");
    await app.restart(() => {
      const after = status();
      assert.equal(after.session.session, before.session.session);
      assert.ok(after.session.stopped_ms >= after.session.started_ms, "window close saves a clean stop before process exit");
      assert.equal(after.latest.session, before.latest.session, "history survived the quit");
    });

    // Clearing needs a pause first; a pause outlives a restart; then clearing empties history.
    const recorded = await query();
    assert.ok(recorded.latest, "history read after the restart");
    assert.match(String(await rejection(app.page, "clear_performance_history", {})), /pause/i, "no clearing while recording");
    const settings = recorded.settings;
    assert.equal(await rejection(app.page, "set_performance_settings", { settings: { ...settings, enabled: false } }), null);
    await app.restart();
    const paused = await query();
    assert.equal(paused.settings.enabled, false, "pause is durable");
    assert.ok(paused.points.some((p) => p.to_ms === recorded.latest.wall_ms), "history survives the second restart");
    assert.equal(await rejection(app.page, "clear_performance_history", {}), null);
    const cleared = await query();
    assert.equal(cleared.latest, null);
    assert.equal(cleared.points.length, 0);
    assert.equal(await rejection(app.page, "set_performance_settings", { settings: { ...settings, enabled: true } }), null);
    assert.equal((await query()).settings.enabled, true);
  } finally {
    await app.close();
  }
});
