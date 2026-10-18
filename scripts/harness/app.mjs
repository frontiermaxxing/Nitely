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
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
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

/** The harness's own nitely, built beside the app: what agents and the crew run in a scenario. */
export const harnessNitely = () => exe("nitely");

/** Runs the harness's nitely CLI on the run's data folder, e.g. to seed it or read it back. */
export function nitely(home, ...args) {
  return run(exe("nitely"), args, { capture: true, env: { ...personsEnv(), NITELY_HOME: home, NITELY_AGENT_HOME: agentsOf(home) } });
}

/** The run's own agents' home, inside its data folder: never the person's ~/.claude, ~/.codex or ~/.cursor. */
const agentsOf = (home) => join(home, "agents");

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
      let holder;
      try {
        holder = Number(readFileSync(LOCK, "utf8").trim());
      } catch (err) {
        if (err.code === "ENOENT") continue; // Released between the two looks: try again.
        throw err;
      }
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
 * Whether this runs in CI (GitHub Actions sets CI=true), where the app's window is on a desktop
 * nobody is using. Everywhere else it is on a person's screen: there a run needs their opt-in, the
 * window never maximizes (window.e2e.mjs), and it never uses the GPU (tauri.harness.conf.json),
 * since a GPU-composited window resizing on a busy desktop is the likeliest trigger of the hard
 * freeze in NIT-188.
 */
export const ON_CI = process.env.CI === "true";

const LOCAL_REFUSED = `The harness app opens a real window on this PC's screen, so it runs in CI by default
(.github/workflows/app.yml runs npm run app:test on every pull request). Run it here only when an
issue needs it, one run at a time, and not while the person is using the PC for something else:
set NITELY_HARNESS_LOCAL=1 for that run (AGENTS.md, "Driving the real app").`;

/**
 * Starts the harness app on a new data folder, seeded by `seed(home)`, with DevTools on a free
 * port. Returns the page, the folder, `restart()`, which closes the app as the person would and
 * starts it again on the same data and storage, and `close()`, which ends the app and removes the
 * folder.
 */
export async function launch({ seed = seedSample, env = {}, keepHome = false } = {}) {
  if (!ON_CI && process.env.NITELY_HARNESS_LOCAL !== "1") throw new Error(LOCAL_REFUSED);
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
    if (web.endsWith(".harness")) rmSync(web, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
  }
  // Agent setup and the token counts read the agents' home; the app refuses to start without one
  // of its own (src-tauri/src/harness.rs).
  const agents = env.NITELY_AGENT_HOME ?? agentsOf(home);
  mkdirSync(agents, { recursive: true });
  seed(home);
  mkdirSync(join(home, "screens"), { recursive: true });
  let log = "";
  // Cleaning up never hides why a run failed: what cannot be removed is left and said.
  const remove = () => {
    try {
      rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
    } catch (e) {
      console.error(`harness: could not remove ${home}: ${e.message}`);
    }
  };
  const app = {
    home,
    log: () => log,
    /** `whileClosed()` runs between the two, e.g. an agent asking something while the app is not up. */
    async restart(whileClosed = () => {}) {
      // Closed by its own window, as the person would, so it saves what it keeps (what it told).
      await app.page.invoke("plugin:window|close", { label: "main" }).catch(() => {});
      const code = await Promise.race([app.exited, new Promise((r) => setTimeout(() => r("running"), 15000))]);
      app.page.close();
      if (code === "running") throw new Error("the app did not end when its window closed");
      await whileClosed();
      await boot();
      return app.page;
    },
    async close() {
      app.page?.close();
      await app.stop?.();
      keepLog(log);
      if (!keepHome) remove();
      unlock();
    },
  };
  const boot = async () => {
    // A port file left by the last run would name a port nobody listens on now.
    const portFile = webviewDir() && join(webviewDir(), "DevToolsActivePort");
    if (portFile) rmSync(portFile, { force: true });
    const child = spawn(exe("nitely-app"), [], {
      // Not in its data folder, which close() removes.
      cwd: ROOT,
      // The harness build asks WebView2 for DevTools on a port of its own choosing (its config's
      // additionalBrowserArgs); WebView2 writes it to DevToolsActivePort in its folder.
      env: { ...personsEnv(), ...env, NITELY_HOME: home, NITELY_AGENT_HOME: agents, NITELY_HARNESS: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout.on("data", (d) => (log += d));
    child.stderr.on("data", (d) => (log += d));
    const exited = new Promise((r) => child.once("exit", r));
    // The app and its WebView2 processes, all of them: a process left behind holds the folders.
    const stop = async () => {
      if (child.exitCode === null) {
        if (process.platform === "win32") spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], { stdio: "ignore" });
        else child.kill();
        await Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]);
      }
    };
    Object.assign(app, { pid: child.pid, exited, stop, page: null });
    try {
      const up = (async () => {
        app.port = await devtoolsPort(90000);
        return Page.connect(app.port, 30000);
      })();
      app.page = await Promise.race([up, exited.then((code) => Promise.reject(new Error(`the app exited (${code}) before its page came up`)))]);
      await app.page.waitFor(`return document.querySelector(".sidebar") !== null;`, { timeoutMs: 30000, what: "the app's sidebar" });
    } catch (e) {
      app.page?.close();
      await stop();
      keepLog(log + diagnose());
      remove();
      throw new Error(`${e.message}\n--- the app's output ---\n${log}${diagnose()}`);
    }
  };
  await boot();
  return app;
}

/**
 * This process's environment, less what would point the app at the person's own things: their
 * agents' session folders (CLAUDE_CONFIG_DIR, CODEX_HOME; the app reads NITELY_AGENT_HOME's
 * instead, and refuses them outside it) and WebView2's extra arguments, which WebView2 adds to the
 * config's, so a debugging port of someone's would override the one the harness lets it choose.
 */
function personsEnv() {
  const env = { ...process.env };
  const drop = ["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "NITELY_AGENT_HOME"];
  for (const name of Object.keys(env)) if (drop.includes(name.toUpperCase())) delete env[name];
  return env;
}

/** WebView2's folder for the harness identifier, where it keeps storage and DevToolsActivePort. */
function webviewDir() {
  return process.env.LOCALAPPDATA ? join(process.env.LOCALAPPDATA, IDENTIFIER, "EBWebView") : null;
}

/** The DevTools port WebView2 chose, once it has written it. */
async function devtoolsPort(timeoutMs) {
  const dir = webviewDir();
  if (!dir) throw new Error("no LOCALAPPDATA: the harness runs on Windows for now");
  const file = join(dir, "DevToolsActivePort");
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    try {
      const port = Number(readFileSync(file, "utf8").split(/\r?\n/)[0]);
      if (port > 0) return port;
    } catch {
      // Not written yet.
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`WebView2 never wrote ${file}`);
}

/** What was running and what WebView2 left, for a launch that failed (CI shows it). */
function diagnose() {
  if (process.platform !== "win32") return "";
  const ps = spawnSync("powershell.exe", ["-NoProfile", "-Command", "Get-Process nitely-app,msedgewebview2 -ErrorAction SilentlyContinue | Format-Table Id,ProcessName,MainWindowTitle -AutoSize | Out-String -Width 200"], { encoding: "utf8" });
  const dir = webviewDir();
  let files = "";
  try {
    files = dir && existsSync(dir) ? readdirSync(dir).join(", ") : `${dir} does not exist`;
  } catch (e) {
    files = String(e);
  }
  return `\n--- processes ---\n${ps.stdout ?? ""}\n--- WebView2 folder ---\n${files}\n`;
}

/** The app's output of the last run, kept in target/harness/app.log (CI uploads it). */
function keepLog(log) {
  try {
    mkdirSync(TARGET, { recursive: true });
    writeFileSync(join(TARGET, "app.log"), log);
  } catch {
    // Only for looking at later.
  }
}

/** The taskbar count the app last set, and whether it drew the overlay: `{ count, drawn }` or null. */
export function badge(home) {
  const file = join(home, "harness-badge.json");
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return null; // Not written yet, or being written.
  }
}

/** The notifications the app asked to show, from its data folder (the harness records them). */
export function notifications(home) {
  const file = join(home, "harness-notifications.jsonl");
  if (!existsSync(file)) return [];
  // Only whole lines: the app may be writing the last one while this reads.
  const lines = readFileSync(file, "utf8").split("\n");
  lines.pop();
  return lines.filter(Boolean).map((l) => JSON.parse(l));
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const what = process.argv[2];
  if (what === "build") build();
  else if (what === "drive") {
    const app = await launch({ keepHome: process.argv.includes("--keep") }).catch((e) => {
      console.error(e.message);
      process.exit(1);
    });
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
