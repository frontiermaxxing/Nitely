// A small driver for the real app's window over the DevTools protocol (WebView2 speaks it like
// Chrome). Plain CDP over Node's WebSocket, no browser automation library. scripts/harness/app.mjs
// starts the app and hands back a Page; AGENTS.md says how to use it.

import { writeFileSync } from "node:fs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The app's page among the DevTools targets on `port`, once it is there. */
export async function findPage(port, timeoutMs = 60000) {
  const until = Date.now() + timeoutMs;
  let last = "no answer";
  while (Date.now() < until) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = targets.find((t) => t.type === "page" && /^(https?:\/\/tauri\.localhost|tauri:\/\/)/.test(t.url));
      if (page) return page;
      last = JSON.stringify(targets.map((t) => t.url));
    } catch (e) {
      last = String(e);
    }
    await sleep(250);
  }
  throw new Error(`the app's page did not appear on DevTools port ${port}: ${last}`);
}

export class Page {
  static async connect(port, timeoutMs) {
    const target = await findPage(port, timeoutMs);
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((resolve, reject) => {
      ws.onopen = resolve;
      ws.onerror = () => reject(new Error(`could not open ${target.webSocketDebuggerUrl}`));
    });
    const page = new Page(ws);
    await page.send("Page.enable");
    await page.send("Runtime.enable");
    return page;
  }

  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    this.closed = false;
    ws.onmessage = (m) => {
      const msg = JSON.parse(m.data);
      const done = msg.id && this.pending.get(msg.id);
      if (!done) return;
      this.pending.delete(msg.id);
      if (msg.error) done.reject(new Error(`${msg.error.message}`));
      else done.resolve(msg.result);
    };
    // The app gone (closed, crashed): every call still waiting fails instead of hanging.
    ws.onclose = () => {
      this.closed = true;
      for (const done of this.pending.values()) done.reject(new Error("the app's page went away"));
      this.pending.clear();
    };
  }

  /** One DevTools call; it fails when the page goes away or gives no answer in `timeoutMs`. */
  send(method, params = {}, { timeoutMs = 30000 } = {}) {
    if (this.closed) return Promise.reject(new Error("the app's page went away"));
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} got no answer in ${timeoutMs}ms`));
      }, timeoutMs);
      const settle = (fn) => (v) => {
        clearTimeout(timer);
        fn(v);
      };
      this.pending.set(id, { resolve: settle(resolve), reject: settle(reject) });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  /** Runs `expression` (an async function body is fine) in the page and returns its JSON value. */
  async eval(expression) {
    const r = await this.send("Runtime.evaluate", { expression: `(async () => { ${expression} })()`, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(`in the page: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value;
  }

  /** Waits until `expression` returns something truthy, and returns it. */
  async waitFor(expression, { timeoutMs = 15000, what = expression } = {}) {
    const until = Date.now() + timeoutMs;
    let last;
    while (Date.now() < until) {
      try {
        last = await this.eval(expression);
        if (last) return last;
      } catch (e) {
        last = String(e);
      }
      await sleep(150);
    }
    throw new Error(`timed out waiting for ${what} (last: ${JSON.stringify(last)})`);
  }

  /** Opens a view by its hash route, e.g. "#inbox" or "#project/APP/board". */
  async open(hash) {
    await this.eval(`location.hash = ${JSON.stringify(hash)}; return true;`);
  }

  /**
   * The centre of the first visible element that matches: `css` a selector, `text` its text or
   * accessible name (exact after trimming, else contained). Null when there is none.
   */
  async find(opts = {}) {
    return this.eval(findBody(opts));
  }

  /** A real pointer click (pressed and released) at the element's centre, as a person's would be. */
  async click(target, { count = 1, timeoutMs = 10000 } = {}) {
    const opts = typeof target === "string" ? { text: target } : target;
    const at = await this.waitFor(findBody(opts), { timeoutMs, what: JSON.stringify(opts) });
    for (let i = 1; i <= count; i++) {
      await this.send("Input.dispatchMouseEvent", { type: "mousePressed", x: at.x, y: at.y, button: "left", clickCount: i });
      await this.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: at.x, y: at.y, button: "left", clickCount: i });
    }
    return at;
  }

  /** Presses a key in the page (not the OS: Alt+Space and Win+arrows do not go through here). */
  async key(key, { code = key, modifiers = 0 } = {}) {
    await this.send("Input.dispatchKeyEvent", { type: "keyDown", key, code, modifiers, text: key.length === 1 ? key : undefined });
    await this.send("Input.dispatchKeyEvent", { type: "keyUp", key, code, modifiers });
  }

  /** The window's contents as PNG, saved to `path`. */
  async screenshot(path) {
    const shot = await this.send("Page.captureScreenshot", { format: "png" });
    writeFileSync(path, Buffer.from(shot.data, "base64"));
    return path;
  }

  /** Calls a backend command the way the page does. */
  async invoke(cmd, args = {}) {
    return this.eval(`return await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)});`);
  }

  /** The native window's state, from Tauri's window API: maximized, minimized, focused, visible. */
  async windowState(label = "main") {
    const ask = (what) => this.invoke(`plugin:window|${what}`, { label });
    const [maximized, minimized, focused, visible] = await Promise.all([ask("is_maximized"), ask("is_minimized"), ask("is_focused"), ask("is_visible")]);
    return { maximized, minimized, focused, visible };
  }

  close() {
    try {
      this.ws.close();
    } catch {
      // Already gone with the app.
    }
  }
}

/** The page-side search behind find and click: visible elements matching a selector and a name. */
function findBody({ css = "button, a, [role=button], [role=tab], [role=option], input, select, li", text } = {}) {
  return `
    const want = ${JSON.stringify(text ?? null)};
    const name = (el) => (el.getAttribute("aria-label") || el.textContent || el.value || "").trim();
    const all = [...document.querySelectorAll(${JSON.stringify(css)})].filter((el) => {
      const r = el.getBoundingClientRect();
      return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== "hidden";
    });
    const el = want === null ? all[0] : (all.find((e) => name(e) === want) ?? all.find((e) => name(e).includes(want)));
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2, name: name(el).slice(0, 80) };`;
}
