// The real app, driven by an agent or a test (AGENTS.md, "Driving the real app").
//
//   node scripts/harness/app.mjs build      the harness build: target/harness/debug/nitely-app.exe
//   node scripts/harness/app.mjs drive      starts it on seeded sample data and prints the DevTools
//                                           port and the scratch data folder; Ctrl+C ends it
//   npm run app:test                        the end-to-end tests in scripts/harness/*.e2e.mjs
//
// It never touches the person's Nitely: the build has its own identifier (so its own single-
// instance lock, WebView2 storage and app data), and every run gets a new data folder in the temp
// dir as NITELY_HOME. The app refuses to start under NITELY_HARNESS=1 without one of its own
// (src-tauri/src/harness.rs), registers no nitely:// links, signs with a throwaway approval key
// and writes its notifications to harness-notifications.jsonl instead of showing them.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Page } from "./cdp.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const TARGET = join(ROOT, "target", "harness");
const IDENTIFIER = "net.quixet.nitely.harness";
const exe = (name) => join(TARGET, "debug", process.platform === "win32" ? `${name}.exe` : name);

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { cwd: ROOT, stdio: opts.capture ? "pipe" : "inherit", encoding: "utf8", shell: false, ...opts });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed (${r.status}): ${r.stderr ?? ""}${r.stdout ?? ""}`);
  return r.stdout ?? "";
}

/**
 * Builds the harness app (the frontend embedded, as a release would be, with a debug Rust build)
 * and the nitely CLI it is seeded with, into target/harness so the dev build is left alone.
 */
export function build() {
  const env = { ...process.env, CARGO_TARGET_DIR: TARGET };
  run(process.execPath, ["node_modules/@tauri-apps/cli/tauri.js", "build", "--debug", "--no-bundle", "--config", "scripts/harness/tauri.harness.conf.json"], { env });
  run("cargo", ["build", "-p", "nitely-cli"], { env });
}

/** The person's own data folder, which the harness must never use. */
function realDataDir() {
  if (process.platform === "win32") return join(process.env.APPDATA ?? join(homedir(), "AppData", "Roaming"), "Nitely");
  if (process.platform === "darwin") return join(homedir(), "Library", "Application Support", "Nitely");
  return join(process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"), "Nitely");
}

const norm = (p) => resolve(p).replace(/\\/g, "/").replace(/\/$/, "").toLowerCase();

function freePort() {
  return new Promise((ok, fail) => {
    const s = createServer();
    s.once("error", fail);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => ok(port));
    });
  });
}

/** Runs the harness's nitely CLI on the run's data folder, e.g. to seed it or read it back. */
export function nitely(home, ...args) {
  return run(exe("nitely"), args, { capture: true, env: { ...process.env, NITELY_HOME: home } });
}

/**
 * The default seed: a project with an issue waiting for approval and a question waiting for an
 * answer, the two decisions the Inbox test acts on.
 */
export function seedSample(home) {
  nitely(home, "project", "create", "APP", "Acme");
  const file = (title, body) => ["issue", "create", "--project", "APP", "--title", title, "--kind", "feature", "--source", "claude", "--evidence", "harness seed", "--body", body, "--priority", "2"];
  nitely(home, ...file("Ship the export button", "Seeded by the harness: waits for approval."));
  nitely(home, ...file("Choose the default theme", "Seeded by the harness: has a question."));
  nitely(home, "issue", "move", "APP-1", "Needs You");
  nitely(home, "ask", "Should the default theme follow the system?", "--issue", "APP-2", "--option", "Follow the system", "--option", "Always light");
}

/**
 * One harness app at a time on a machine: they share an identifier, so a second would only hand
 * over to the first (single instance) and share its WebView2 storage. A launch waits for a running
 * one to end, across worktrees; a lock whose process is gone is taken over.
 */
const LOCK = join(tmpdir(), "nitely-harness.lock");

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === "EPERM";
  }
}

async function takeLock(waitMs = 10 * 60 * 1000) {
  const until = Date.now() + waitMs;
  for (;;) {
    try {
      writeFileSync(LOCK, String(process.pid), { flag: "wx" });
      return () => {
        try {
          if (readFileSync(LOCK, "utf8") === String(process.pid)) rmSync(LOCK, { force: true });
        } catch {
          // Already gone.
        }
      };
    } catch (e) {
      if (e.code !== "EEXIST") throw e;
      const holder = Number(readFileSync(LOCK, "utf8").trim());
      if (!holder || !alive(holder)) {
        rmSync(LOCK, { force: true });
        continue;
      }
      if (Date.now() > until) throw new Error(`another harness (process ${holder}) has run for over ${waitMs / 60000} minutes; end it or remove ${LOCK}`);
      await new Promise((r) => setTimeout(r, 1000));
    }
  }
}

/**
 * Starts the harness app on a new data folder, seeded by `seed(home)`, with DevTools on a free
 * port. Returns the page, the folder and `close()`, which ends the app and removes the folder.
 */
export async function launch({ seed = seedSample, env = {}, keepHome = false } = {}) {
  if (!existsSync(exe("nitely-app"))) throw new Error(`no harness build at ${exe("nitely-app")}: run \`node scripts/harness/app.mjs build\` first`);
  const unlock = await takeLock();
  try {
    return await start({ seed, env, keepHome, unlock });
  } catch (e) {
    unlock();
    throw e;
  }
}

async function start({ seed, env, keepHome, unlock }) {
  const home = mkdtempSync(join(tmpdir(), "nitely-harness-"));
  const real = norm(realDataDir());
  if (norm(home) === real || norm(home).startsWith(`${real}/`)) throw new Error(`refusing to run on the real data folder ${real}`);
  // Its WebView2 storage (notifications told, settings) starts empty every run; the folder is the
  // harness identifier's own, never the app's.
  if (process.platform === "win32" && process.env.LOCALAPPDATA) {
    const web = join(process.env.LOCALAPPDATA, IDENTIFIER);
    if (web.endsWith(".harness")) rmSync(web, { recursive: true, force: true });
  }
  seed(home);
  const port = await freePort();
  mkdirSync(join(home, "screens"), { recursive: true });
  const child = spawn(exe("nitely-app"), [], {
    cwd: home,
    env: { ...process.env, ...env, NITELY_HOME: home, NITELY_HARNESS: "1", WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => (log += d));
  child.stderr.on("data", (d) => (log += d));
  const exited = new Promise((r) => child.once("exit", r));
  let page;
  try {
    page = await Promise.race([Page.connect(port, 90000), exited.then((code) => Promise.reject(new Error(`the app exited (${code}) before its page came up:\n${log}`)))]);
    await page.waitFor(`return document.querySelector(".sidebar") !== null;`, { timeoutMs: 30000, what: "the app's sidebar" });
  } catch (e) {
    child.kill();
    rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    throw e;
  }
  const close = async () => {
    page?.close();
    if (child.exitCode === null) {
      if (process.platform === "win32") spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], { stdio: "ignore" });
      else child.kill();
      await Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]);
    }
    if (!keepHome) rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    unlock();
  };
  return { page, home, port, pid: child.pid, log: () => log, exited, close };
}

/** The taskbar count the app last set, and whether it drew the overlay: `{ count, drawn }` or null. */
export function badge(home) {
  const file = join(home, "harness-badge.json");
  return existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) : null;
}

/** The notifications the app asked to show, from its data folder (the harness records them). */
export function notifications(home) {
  const file = join(home, "harness-notifications.jsonl");
  return existsSync(file) ? readFileSync(file, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [];
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const what = process.argv[2];
  if (what === "build") build();
  else if (what === "drive") {
    const app = await launch({ keepHome: process.argv.includes("--keep") });
    console.log(JSON.stringify({ devtools: `http://127.0.0.1:${app.port}`, home: app.home, pid: app.pid }, null, 2));
    console.log("The app runs on throwaway data. Drive it with scripts/harness/cdp.mjs (Page.connect(port)). Ctrl+C ends it.");
    const stop = async () => {
      await app.close();
      process.exit(0);
    };
    process.on("SIGINT", stop);
    process.on("SIGTERM", stop);
    await app.exited;
    await app.close();
  } else {
    console.error("usage: node scripts/harness/app.mjs build | drive [--keep]");
    process.exit(1);
  }
}
