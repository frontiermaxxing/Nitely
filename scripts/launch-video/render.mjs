// Renders the launch video: index.html drawn frame by frame in headless Chrome over CDP, piped into
// ffmpeg, then joined with the score synth.cjs writes. README.md says what each step needs.
//
//   node scripts/launch-video/render.mjs all               score, picture, final cut, 720p preview
//   node scripts/launch-video/render.mjs stills 1.5 16.2   out/still-<t>.png, to check single frames
//   node scripts/launch-video/render.mjs score             out/score.wav only
//   node scripts/launch-video/render.mjs video [from] [to] out/video-silent.mp4 only (60 fps)
//   node scripts/launch-video/render.mjs mux               out/nitely-launch.mp4 and -720p.mp4
//
// CHROME names the browser when it is not in a usual place; ffmpeg must be on PATH.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "out");
mkdirSync(out, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const FPS = 60;
const CUES = new Function(`${readFileSync(join(here, "cues.js"), "utf8")}; return CUES;`)();

function ffmpeg(args) {
  const r = spawnSync("ffmpeg", ["-y", "-loglevel", "error", ...args], { stdio: "inherit" });
  if (r.error) throw new Error(`ffmpeg did not run (${r.error.message}); install it and put it on PATH`);
  if (r.status !== 0) throw new Error(`ffmpeg exited ${r.status}`);
}

function findChrome() {
  const candidates = [
    process.env.CHROME,
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser",
  ].filter(Boolean);
  const found = candidates.find((p) => existsSync(p));
  if (!found) throw new Error("no Chrome or Edge found; set CHROME to its path");
  return found;
}

/** A headless browser on index.html, with seek(t) ready to draw any frame. */
async function openPage() {
  const profile = join(out, "profiles", `p${process.pid}`);
  mkdirSync(profile, { recursive: true });
  // No GPU: the frames are drawn in software, the same on every machine, and nothing opens on screen.
  const chrome = spawn(findChrome(), [
    "--headless=new", "--disable-gpu", "--hide-scrollbars", "--mute-audio", "--no-first-run",
    "--allow-file-access-from-files", "--force-device-scale-factor=1", "--window-size=1920,1080",
    `--user-data-dir=${profile}`, "--remote-debugging-port=0", "about:blank",
  ], { stdio: "ignore" });
  let port;
  for (let i = 0; i < 200 && !port; i++) {
    const f = join(profile, "DevToolsActivePort");
    if (existsSync(f)) port = readFileSync(f, "utf8").split("\n")[0].trim();
    else await sleep(100);
  }
  if (!port) throw new Error("the browser did not open its DevTools port");
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data), p = msg.id && pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    msg.error ? p.rej(new Error(msg.error.message)) : p.res(msg.result);
  };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
  const evaluate = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
    return r.result.value;
  };
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: pathToFileURL(join(here, "index.html")).href });
  for (let i = 0; i < 100; i++) { try { if (await evaluate("!!window.ready")) break; } catch {} await sleep(100); }
  await evaluate("window.ready");
  await sleep(300);
  const clip = { x: 0, y: 0, width: 1920, height: 1080, scale: 1 };
  return {
    async shot(t, format = "png") {
      await evaluate(`seek(${t}, ${Math.round(t * FPS)})`);
      const r = await send("Page.captureScreenshot", { format, quality: format === "jpeg" ? 96 : undefined, clip, optimizeForSpeed: format !== "png" });
      return Buffer.from(r.data, "base64");
    },
    async close() {
      ws.close();
      // Ask the browser itself to quit: on Windows the process spawned here hands off to others, so
      // killing it leaves the real browser and its helpers running.
      try {
        const { webSocketDebuggerUrl } = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
        const b = new WebSocket(webSocketDebuggerUrl);
        await new Promise((r, j) => { b.onopen = r; b.onerror = j; });
        b.send(JSON.stringify({ id: 1, method: "Browser.close" }));
        await new Promise((r) => { b.onclose = r; setTimeout(r, 3000); });
      } catch {
        chrome.kill();
      }
      // Its crash reporter can hold a file a moment longer; a profile left behind is only in out/.
      for (let i = 0; i < 20; i++) {
        try { rmSync(profile, { recursive: true, force: true }); break; } catch { await sleep(250); }
      }
    },
  };
}

async function stills(times) {
  const page = await openPage();
  try {
    for (const a of times) { writeFileSync(join(out, `still-${a}.png`), await page.shot(parseFloat(a))); console.log(`out/still-${a}.png`); }
  } finally { await page.close(); }
}

async function video(from = 0, to = CUES.duration) {
  const page = await openPage();
  const ff = spawn("ffmpeg", ["-y", "-loglevel", "error", "-f", "image2pipe", "-framerate", String(FPS), "-c:v", "mjpeg", "-i", "-",
    "-c:v", "libx264", "-preset", "slow", "-crf", "20", "-pix_fmt", "yuv420p", join(out, "video-silent.mp4")], { stdio: ["pipe", "inherit", "inherit"] });
  const n = Math.round((to - from) * FPS), t0 = Date.now();
  try {
    for (let i = 0; i < n; i++) {
      const buf = await page.shot(from + i / FPS, "jpeg");
      if (!ff.stdin.write(buf)) await new Promise((r) => ff.stdin.once("drain", r));
      if (i % 240 === 0) console.log(`frame ${i}/${n}, ${((Date.now() - t0) / (i + 1)).toFixed(0)} ms a frame`);
    }
  } finally {
    ff.stdin.end();
    await page.close();
  }
  await new Promise((r) => ff.on("close", r));
  console.log(`out/video-silent.mp4 in ${((Date.now() - t0) / 1000).toFixed(0)} s`);
}

function score() {
  const r = spawnSync(process.execPath, [join(here, "synth.cjs")], { stdio: "inherit" });
  if (r.status !== 0) throw new Error("synth.cjs failed");
}

function mux() {
  // Loudness for the web: -14 LUFS integrated, -1 dB true peak.
  ffmpeg(["-i", join(out, "score.wav"), "-af", "loudnorm=I=-14:TP=-1:LRA=11", "-ar", "48000", join(out, "score-norm.wav")]);
  ffmpeg(["-i", join(out, "video-silent.mp4"), "-i", join(out, "score-norm.wav"), "-map", "0:v", "-map", "1:a", "-c:v", "copy",
    "-c:a", "aac", "-b:a", "256k", "-shortest", "-movflags", "+faststart", join(out, "nitely-launch.mp4")]);
  // A copy small enough to send around (about 25 MB).
  ffmpeg(["-i", join(out, "nitely-launch.mp4"), "-vf", "scale=1280:720:flags=lanczos", "-c:v", "libx264", "-preset", "slow", "-b:v", "2900k",
    "-maxrate", "3500k", "-bufsize", "7000k", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart", join(out, "nitely-launch-720p.mp4")]);
  console.log("out/nitely-launch.mp4, out/nitely-launch-720p.mp4");
}

const [mode, ...args] = process.argv.slice(2);
if (mode === "stills") await stills(args);
else if (mode === "score") score();
else if (mode === "video") await video(args[0] ? parseFloat(args[0]) : 0, args[1] ? parseFloat(args[1]) : CUES.duration);
else if (mode === "mux") mux();
else if (mode === "all") { score(); await video(); mux(); }
else {
  console.error("usage: node scripts/launch-video/render.mjs all | stills <t>... | score | video [from] [to] | mux");
  process.exit(2);
}
