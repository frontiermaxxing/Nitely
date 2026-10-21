// The window's own title bar on Windows (NIT-89, live check NIT-110), in the real app, checked
// through the window's own state (NIT-548): the caption buttons, the drag strip's double-click, the
// dimming when the window has no focus, and closing. What needs OS input (dragging the window, Alt+Space,
// Win+arrows, resizing by the border) is not sent from here; NIT-110 lists it. Maximizing,
// restoring and minimizing run only in CI: on a person's PC a window changing size is the likeliest
// trigger of the freeze in NIT-188, so there only what leaves the window as it is runs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { ON_CI, launch } from "./app.mjs";

const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test("the in-app title bar: buttons, double-click, focus dimming, close", { timeout: 180000, skip: process.platform !== "win32" && "the app draws its own title bar on Windows only" }, async () => {
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
    await page.screenshot(join(SCREENS, "titlebar-restored.png"));

    const strip = await page.eval(`const r = document.querySelector(".titlebar-drag").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 };`);
    const brand = await page.eval(`const el = document.querySelector(".brand[data-tauri-drag-region]"); if (!el) return null; const r = el.getBoundingClientRect(); return { x: r.left + 8, y: r.top + r.height / 2 };`);
    assert.ok(brand, "the brand row is a drag strip");
    if (ON_CI) {
      // Maximize and restore with the button; the icon and name follow.
      await page.click({ css: "[data-test=maximize]" });
      await until((s) => s.maximized, "maximized");
      await page.screenshot(join(SCREENS, "titlebar-maximized.png"));
      await page.click({ css: "[data-test=maximize]" });
      await until((s) => !s.maximized, "restored");

      // A double-click on the strip above the page maximizes and restores, as a native bar does. Only
      // the second press is sent (a first one would start an OS drag of the window).
      const doubleClick = async () => {
        await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: strip.x, y: strip.y, button: "left", clickCount: 2 });
        await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: strip.x, y: strip.y, button: "left", clickCount: 2 });
      };
      await doubleClick();
      await until((s) => s.maximized, "maximized by a double-click");
      await doubleClick();
      await until((s) => !s.maximized, "restored by a double-click");

      // The sidebar's brand row is the other strip: the same double-click there.
      Object.assign(strip, brand);
      await doubleClick();
      await until((s) => s.maximized, "maximized by a double-click on the brand row");
      await doubleClick();
      await until((s) => !s.maximized, "restored by a double-click on the brand row");
    } else {
      console.log("not in CI: maximize, restore and the double-clicks are left to CI (NIT-188)");
    }

    // The buttons are dimmed exactly when the window has no focus; minimized, it has none.
    const dimmedMatches = async () => {
      const s = await state();
      const dimmed = await page.eval(`return document.querySelector(".caption").classList.contains("caption-blurred");`);
      return { focused: s.focused, dimmed };
    };
    let f = await dimmedMatches();
    assert.equal(f.dimmed, !f.focused, `dimmed when unfocused: ${JSON.stringify(f)}`);
    if (ON_CI) {
      await page.click({ css: "[data-test=minimize]" });
      await until((s) => s.minimized, "minimized");
      await page.waitFor(`return document.querySelector(".caption").classList.contains("caption-blurred");`, { what: "dimmed buttons while minimized" });
      f = await dimmedMatches();
      assert.deepEqual(f, { focused: false, dimmed: true });
    }

    // Close ends the app.
    await page.click({ css: "[data-test=close]" }).catch(() => {});
    const code = await Promise.race([app.exited, sleep(10000).then(() => "still running")]);
    assert.notEqual(code, "still running", "Close ends the app");
  } catch (e) {
    console.error(app.log());
    throw e;
  } finally {
    await app.close();
  }
});
