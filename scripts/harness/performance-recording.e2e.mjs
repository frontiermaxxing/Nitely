import { test } from "node:test";
import assert from "node:assert/strict";
import { launch, nitely } from "./app.mjs";

test("a normal desktop quit records a clean collector stop before restarting", { timeout: 120000 }, async () => {
  const app = await launch();
  const status = () => JSON.parse(nitely(app.home, "performance", "status"));
  try {
    let before;
    const deadline = Date.now() + 15000;
    while (!(before = status()).latest && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 100));
    assert.ok(before.latest, "desktop collected a durable sample");
    assert.equal(before.session.stopped_ms, null, "desktop owns the active session");
    await app.restart(() => {
      const after = status();
      assert.equal(after.session.session, before.session.session);
      assert.ok(after.session.stopped_ms >= after.session.started_ms, "window close saves a clean stop before process exit");
      assert.equal(after.latest.session, before.latest.session, "history survived the quit");
    });
  } finally {
    await app.close();
  }
});
