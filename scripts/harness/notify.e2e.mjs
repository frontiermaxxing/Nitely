// Notifications for decisions (NIT-124, live check NIT-126), in the real app: a question asked
// while the person is elsewhere is told once, two at once come as one, the taskbar count follows
// the Inbox, and a toast's click opens that decision. The harness records the toasts instead of
// showing them (NITELY_HARNESS_TOASTS=1 shows them too); clicking a real toast is not sent from here.
// Nothing is told while the Inbox is in view, in quiet hours, or again after a restart.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { badge, launch, nitely, notifications } from "./app.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
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

test("a new decision is told once, a batch as one, the count follows, and a click opens it", { timeout: 180000 }, async () => {
  const app = await launch();
  const { page, home } = app;
  try {
    // Away from the Inbox; what waited at start (two seeded decisions) is not told.
    await page.open("#overview");
    await until(() => badge(home)?.count === 2, "the taskbar count of the two seeded decisions");
    await sleep(5000);
    assert.equal(notifications(home).length, 0, "nothing already waiting is told at start");

    // One new question: one toast, saying which issue and what it asks.
    nitely(home, "ask", "Ship it behind a flag?", "--issue", "APP-1", "--option", "Yes", "--option", "No");
    const one = await until(() => notifications(home)[0], "the toast for the new question");
    assert.equal(one.title, "A question for you");
    assert.equal(one.body, "APP-1 asks: Ship it behind a flag?");
    assert.match(one.decision, /^ask-\d+$/);
    assert.equal((await until(() => badge(home)?.count === 3 && badge(home), "the count of three")).drawn, true, "the overlay is drawn");

    // Two more at once: one toast for both.
    nitely(home, "ask", "Keep the old export too?", "--issue", "APP-2");
    nitely(home, "ask", "Name the button Export or Download?", "--issue", "APP-2");
    const batch = await until(() => notifications(home)[1], "the toast for the two new questions");
    assert.equal(batch.title, "2 decisions waiting");
    assert.equal(batch.decision, null);
    await sleep(6000);
    assert.equal(notifications(home).length, 2, "each decision is told once");

    // The toast's click (the event the OS toast sends) opens the Inbox on that question.
    await page.invoke("plugin:event|emit", { event: "nitely://open-decision", payload: one.decision });
    await page.waitFor(`return location.hash.startsWith("#inbox") && document.querySelector(".inbox-detail .decision-matter")?.textContent;`, { what: "the Inbox on the question" });
    const matter = await page.eval(`return document.querySelector(".inbox-detail .decision-matter").textContent;`);
    assert.equal(matter, "Ship it behind a flag?");
    await page.screenshot(join(SCREENS, "notify-opened.png"));

    // Answered in the Inbox, the count goes down.
    await page.click({ css: ".dd-option", text: "Yes" });
    await until(() => badge(home)?.count === 4, "the count after answering", 30000).catch(async () => {
      // Five were waiting (two seeded, three asked); one answered leaves four.
      throw new Error(`count is ${JSON.stringify(badge(home))}`);
    });
  } catch (e) {
    console.error(app.log());
    console.error(JSON.stringify(notifications(home)));
    throw e;
  } finally {
    await app.close();
  }
});

test("nothing is told while the Inbox is in view or in quiet hours, nor again after a restart", { timeout: 240000 }, async () => {
  const app = await launch();
  const { home } = app;
  let page = app.page;
  const ask = (question) => nitely(home, "ask", question, "--issue", "APP-1");
  const countIs = (n) => until(() => badge(home)?.count === n, `the taskbar count of ${n}`);
  const told = () => notifications(home).length;
  // The quiet-hours switch in Settings' Notifications, reached from the Inbox's link, and the
  // hours around now.
  const quietHours = async (on) => {
    await page.open("#inbox");
    await page.click({ css: ".inbox-notify-link" });
    await page.waitFor(`return location.hash === "#settings/notifications" && document.querySelector(".notify-quiet input[type=checkbox]") !== null;`, { what: "Settings on its Notifications" });
    // Settings brings the section back into view while the sections above it load; a click while
    // it moves could land elsewhere.
    await sleep(2000);
    if ((await page.eval(`return document.querySelector(".notify-quiet input[type=checkbox]").checked;`)) !== on) await page.click({ css: ".notify-quiet input[type=checkbox]" });
    if (on) {
      const hhmm = (d) => d.toTimeString().slice(0, 5);
      const from = hhmm(new Date(Date.now() - 60000));
      const to = hhmm(new Date(Date.now() + 30 * 60000));
      // A time input takes no typed text over CDP; set it as the browser would and say so to React.
      await page.eval(`
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
        for (const [label, value] of [["Quiet from", ${JSON.stringify(from)}], ["Quiet until", ${JSON.stringify(to)}]]) {
          const input = document.querySelector('input[aria-label="' + label + '"]');
          set.call(input, value);
          input.dispatchEvent(new Event("input", { bubbles: true }));
        }
        return true;`);
      await page.waitFor(`return document.querySelector('input[aria-label="Quiet until"]').value === ${JSON.stringify(to)};`, { what: "the quiet hours set" });
    }
    await page.waitFor(`return JSON.parse(localStorage.getItem("nitely.notify.settings") ?? "{}").quiet?.on === ${on};`, { what: `quiet hours ${on ? "on" : "off"} saved` });
    await page.open("#overview");
  };
  try {
    await countIs(2);

    // In the Inbox with the window focused, a new question shows there and is not told. Focus is
    // emulated in the page: the harness never takes the person's focus.
    await page.open("#inbox");
    await page.send("Emulation.setFocusEmulationEnabled", { enabled: true });
    assert.equal(await page.eval(`window.dispatchEvent(new Event("focus")); return document.hasFocus() && document.visibilityState;`), "visible", "the page is focused and visible");
    ask("Keep the old export too?");
    await countIs(3);
    await sleep(6000);
    await page.send("Emulation.setFocusEmulationEnabled", { enabled: false });
    await page.eval(`window.dispatchEvent(new Event("blur")); return true;`);
    await page.open("#overview");
    await sleep(6000);
    assert.equal(told(), 0, "a question that arrived while the Inbox was in view is not told, then or later");

    // In quiet hours, away from the Inbox: the count changes, nothing is told.
    await quietHours(true);
    ask("Name the button Export or Download?");
    await countIs(4);
    await sleep(6000);
    assert.equal(told(), 0, "nothing is told in quiet hours");
    await quietHours(false);
    await sleep(6000);
    assert.equal(told(), 0, "nor once they end");

    // Closed and started again: what it already knew is not told again, and a question asked while
    // it was closed is (so what it told survived the restart, rather than starting over).
    page = await app.restart(() => ask("Asked while the app was closed?"));
    await countIs(5);
    const one = await until(() => notifications(home)[0], "the toast for the question asked while closed");
    assert.equal(one.title, "A question for you");
    assert.equal(one.body, "APP-1 asks: Asked while the app was closed?");
    await sleep(6000);
    assert.equal(told(), 1, "after a restart only the new question is told");
  } catch (e) {
    console.error(app.log());
    console.error(JSON.stringify(notifications(home)));
    throw e;
  } finally {
    await app.close();
  }
});
