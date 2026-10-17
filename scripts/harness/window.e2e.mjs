// The window's own title bar on Windows (NIT-89, live check NIT-110), in the real app: the caption
// buttons, the drag strip's double-click, nothing clickable under the strip, the dimming when the
// window has no focus, and closing. What needs OS input (dragging the window, Alt+Space,
// Win+arrows, resizing by the border) is not sent from here; NIT-110 lists it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { launch } from "./app.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test("the in-app title bar: buttons, double-click, the strip's reach, focus dimming, close", { timeout: 180000, skip: process.platform !== "win32" && "the app draws its own title bar on Windows only" }, async () => {
  const app = await launch();
  const { page } = app;
  const state = () => page.windowState();
  const until = async (want, what) => {
    for (let i = 0; i < 40; i++) {
      const s = await state();
      if (want(s)) return s;
      await sleep(150);
    }
    throw new Error(`the window never got ${what}: ${JSON.stringify(await state())}`);
  };
  try {
    await page.open("#inbox");
    const names = await page.eval(`return [...document.querySelectorAll(".caption .caption-btn")].map((b) => b.getAttribute("aria-label"));`);
    assert.deepEqual(names, ["Minimize", "Maximize", "Close"], "three caption buttons with accessible names");
    await page.screenshot(join(SCREENS, "titlebar-restored.png"));

    // Maximize and restore with the button; the icon and name follow.
    await page.click({ css: ".caption-btn", text: "Maximize" });
    await until((s) => s.maximized, "maximized");
    await page.waitFor(`return document.querySelector(".caption-btn[aria-label=Restore]") !== null;`, { what: "the Restore button" });
    await page.screenshot(join(SCREENS, "titlebar-maximized.png"));
    await page.click({ css: ".caption-btn", text: "Restore" });
    await until((s) => !s.maximized, "restored");

    // A double-click on the strip above the page maximizes and restores, as a native bar does. Only
    // the second press is sent (a first one would start an OS drag of the window).
    const strip = await page.eval(`const r = document.querySelector(".titlebar-drag").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 };`);
    const doubleClick = async () => {
      await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: strip.x, y: strip.y, button: "left", clickCount: 2 });
      await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: strip.x, y: strip.y, button: "left", clickCount: 2 });
    };
    await doubleClick();
    await until((s) => s.maximized, "maximized by a double-click");
    await doubleClick();
    await until((s) => !s.maximized, "restored by a double-click");

    // Nothing that takes clicks sits under the strip or the buttons: the board's tabs and filters.
    await page.open("#project/APP/board");
    await page.waitFor(`return document.querySelector(".board, .ui-empty, [role=tablist], .tabs") !== null;`, { what: "the project page" });
    const covered = await page.eval(`
      const hits = [...document.querySelectorAll(".content button, .content select, .content a, .content input")].filter((el) => {
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.top < 40;
      });
      return hits.filter((el) => {
        const r = el.getBoundingClientRect();
        const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return !(at === el || el.contains(at));
      }).map((el) => el.textContent.trim() || el.getAttribute("aria-label"));
    `);
    assert.deepEqual(covered, [], "controls near the top are not under the strip");
    await page.click("Board");
    await page.screenshot(join(SCREENS, "titlebar-board.png"));

    // The buttons are dimmed exactly when the window has no focus; minimized, it has none.
    const dimmedMatches = async () => {
      const s = await state();
      const dimmed = await page.eval(`return document.querySelector(".caption").classList.contains("caption-blurred");`);
      return { focused: s.focused, dimmed };
    };
    let f = await dimmedMatches();
    assert.equal(f.dimmed, !f.focused, `dimmed when unfocused: ${JSON.stringify(f)}`);
    await page.click({ css: ".caption-btn", text: "Minimize" });
    await until((s) => s.minimized, "minimized");
    await page.waitFor(`return document.querySelector(".caption").classList.contains("caption-blurred");`, { what: "dimmed buttons while minimized" });
    f = await dimmedMatches();
    assert.deepEqual(f, { focused: false, dimmed: true });

    // Close ends the app.
    await page.click({ css: ".caption-btn", text: "Close" }).catch(() => {});
    const code = await Promise.race([app.exited, sleep(10000).then(() => "still running")]);
    assert.notEqual(code, "still running", "Close ends the app");
  } catch (e) {
    console.error(app.log());
    throw e;
  } finally {
    await app.close();
  }
});
