// Notifications for decisions (NIT-124, live check NIT-126), in the real app, checked through what
// it tells (the harness records its toasts in harness-notifications.jsonl; NITELY_HARNESS_TOASTS=1
// shows them too) and its taskbar count (NIT-548: no page text). A question asked while the person
// is elsewhere is told once, two at once come as one, the count follows, a toast's click opens the
// Inbox, and nothing is told while the Inbox is in view, in quiet hours, or again after a restart.
// Each "nothing was told" is shown by order, never by waiting: the next question that must be told
// is the next toast, alone.

import { test } from "node:test";
import assert from "node:assert/strict";
import { badge, launch, nitely, notifications } from "./app.mjs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(read, what, ms = 20000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const v = read();
    if (v) return v;
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}`);
}

test("a decision is told once, a batch as one, the count follows, and nothing in view, in quiet hours or after a restart", { timeout: 240000 }, async () => {
  const app = await launch();
  const { home } = app;
  const ask = (question) => nitely(home, "ask", question, "--issue", "APP-1");
  const countIs = (n) => until(() => badge(home)?.count === n && badge(home), `the taskbar count of ${n}`);
  /** The next toast, which must be the only one since the last. */
  const nextToast = async (index, what) => {
    const toast = await until(() => notifications(home)[index], what);
    assert.equal(notifications(home).length, index + 1, `only ${what} was told`);
    return toast;
  };
  // Quiet hours through the setting the Settings page saves, not its switch.
  const quietHours = (on) => {
    const hhmm = (d) => d.toTimeString().slice(0, 5);
    const quiet = { on, from: hhmm(new Date(Date.now() - 60000)), to: hhmm(new Date(Date.now() + 30 * 60000)) };
    return app.page.eval(`
      const key = "nitely.notify.settings";
      const saved = JSON.parse(localStorage.getItem(key) ?? "{}");
      localStorage.setItem(key, JSON.stringify({ ...saved, quiet: ${JSON.stringify(quiet)} }));
      window.dispatchEvent(new Event("nitely:notify-settings"));
      return true;`);
  };
  try {
    // Away from the Inbox; the two seeded decisions are counted, never told.
    await app.page.open("#overview");
    await countIs(2);

    // One new question: one toast, saying which issue and what it asks; it is the first told.
    nitely(home, "ask", "Ship it behind a flag?", "--issue", "APP-1", "--option", "Yes", "--option", "No");
    const one = await nextToast(0, "the toast for the new question");
    assert.equal(one.title, "A question for you");
    assert.equal(one.body, "APP-1 asks: Ship it behind a flag?");
    assert.match(one.decision, /^ask-\d+$/);
    assert.equal((await countIs(3)).drawn, true, "the overlay is drawn");

    // Two more at once: one toast for both.
    ask("Keep the old export too?");
    ask("Name the button Export or Download?");
    const batch = await nextToast(1, "the toast for the two new questions");
    assert.equal(batch.title, "2 decisions waiting");
    assert.equal(batch.decision, null);

    // The toast's click (the event the OS toast sends) opens the Inbox; answered, the count goes down.
    await app.page.invoke("plugin:event|emit", { event: "nitely://open-decision", payload: one.decision });
    await app.page.waitFor(`return location.hash.startsWith("#inbox");`, { what: "the Inbox opened" });
    await app.page.invoke("answer_ask", { id: Number(one.decision.slice("ask-".length)), answer: "Yes" });
    await countIs(4);

    // In the Inbox with the window focused, a new question shows there and is not told. Focus is
    // emulated in the page: the harness never takes the person's focus.
    await app.page.send("Emulation.setFocusEmulationEnabled", { enabled: true });
    assert.equal(await app.page.eval(`window.dispatchEvent(new Event("focus")); return document.hasFocus() && document.visibilityState;`), "visible", "the page is focused and visible");
    ask("Asked while the Inbox is in view?");
    await countIs(5);
    await app.page.send("Emulation.setFocusEmulationEnabled", { enabled: false });
    await app.page.eval(`window.dispatchEvent(new Event("blur")); return true;`);
    await app.page.open("#overview");
    ask("Asked once the Inbox was left?");
    assert.equal((await nextToast(2, "the toast for the question asked after the Inbox")).body, "APP-1 asks: Asked once the Inbox was left?");

    // In quiet hours the count changes and nothing is told, then or once they end.
    await quietHours(true);
    ask("Asked in quiet hours?");
    await countIs(7);
    await quietHours(false);
    ask("Asked after quiet hours?");
    assert.equal((await nextToast(3, "the toast after quiet hours")).body, "APP-1 asks: Asked after quiet hours?");

    // Closed and started again: only the question asked while it was closed is told, so what it
    // told survived the restart rather than starting over.
    await app.restart(() => ask("Asked while the app was closed?"));
    await countIs(9);
    const closed = await nextToast(4, "the toast for the question asked while closed");
    assert.equal(closed.title, "A question for you");
    assert.equal(closed.body, "APP-1 asks: Asked while the app was closed?");
  } catch (e) {
    console.error(app.log());
    console.error(JSON.stringify(notifications(home)));
    throw e;
  } finally {
    await app.close();
  }
});
