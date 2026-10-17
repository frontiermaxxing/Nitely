// Notifications for decisions (NIT-124, live check NIT-126), in the real app: a question asked
// while the person is elsewhere is told once, two at once come as one, the taskbar count follows
// the Inbox, and a toast's click opens that decision. The harness records the toasts instead of
// showing them (NITELY_HARNESS_TOASTS=1 shows them too); clicking a real toast is not sent from here.

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
