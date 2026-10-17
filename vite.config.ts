import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

/** The header the page must send, so another website cannot make a browser call this. */
const LIVE_HEADER = "x-nitely-preview";

/**
 * The browser preview's live mode (`?data=live`): the page POSTs `{cmd, args}` to
 * /__nitely/invoke and gets back what `nitely ui <cmd> <args>` prints for the real database.
 * `nitely ui` runs reads only and refuses anything that changes Nitely, itself; this just relays.
 * Dev server only. It runs this checkout's `target/debug/nitely` when built, else `nitely` on
 * PATH; NITELY_BIN overrides both.
 */
function liveData(): Plugin {
  const local = resolve("target/debug/nitely" + (process.platform === "win32" ? ".exe" : ""));
  const bin = process.env.NITELY_BIN ?? (existsSync(local) ? local : "nitely");
  return {
    name: "nitely-live-data",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use("/__nitely/invoke", (req, res) => {
        // A spawn that fails reports both `error` and `close`; answer once.
        let answered = false;
        const reply = (status: number, body: unknown) => {
          if (answered) return;
          answered = true;
          res.statusCode = status;
          res.setHeader("Content-Type", "application/json");
          res.end(typeof body === "string" ? body : JSON.stringify(body));
        };
        if (req.method !== "POST" || req.headers[LIVE_HEADER] !== "1") return reply(400, { error: "not a preview call" });
        let body = "";
        req.on("data", (chunk) => (body += chunk));
        req.on("end", () => {
          let cmd = "";
          let args: unknown = {};
          try {
            ({ cmd, args = {} } = JSON.parse(body));
          } catch {
            return reply(400, { error: "the body is not JSON" });
          }
          let child;
          try {
            child = spawn(bin, ["ui", String(cmd), JSON.stringify(args)], { windowsHide: true });
          } catch (e) {
            // spawn throws at once on arguments it cannot pass (a NUL byte, say).
            return reply(400, { error: `Could not run nitely ui: ${(e as Error).message}` });
          }
          // Kept as bytes until the end, so a character split across chunks is decoded whole.
          const out: Buffer[] = [];
          const err: Buffer[] = [];
          child.stdout.on("data", (c: Buffer) => out.push(c));
          child.stderr.on("data", (c: Buffer) => err.push(c));
          child.on("error", (e) => reply(500, { error: `Could not run ${bin}: ${e.message}. Run \`cargo build -p nitely-cli\`, or set NITELY_BIN.` }));
          child.on("close", (code) => {
            const text = (b: Buffer[]) => Buffer.concat(b).toString("utf8");
            if (code === 0) reply(200, text(out));
            // 2: refused because it would change something.
            else reply(code === 2 ? 403 : 500, { error: text(err).trim() || `nitely ui exited with ${code}` });
          });
        });
      });
    },
  };
}

// Tauri expects a fixed port and fails if it is taken, rather than drifting.
export default defineConfig({
  plugins: [react(), liveData()],
  clearScreen: false,
  // Two pages: the desktop app, and the phone page the app's phone server serves (docs/phone.md).
  build: { rollupOptions: { input: { main: resolve("index.html"), phone: resolve("phone.html") } } },
  server: { port: 1420, strictPort: true, watch: { ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"] } },
});
