// `node scripts/app-icon.mjs`: makes the app icon's files in src-tauri/icons from its two drawings.
// icon.svg is the mark at full size; icon-small.svg is the same mark drawn for 16 to 48px (less
// margin, a heavier line, brighter outer stations), where the full-size one fades to a single dot.
// `tauri icon` renders each; icon.ico then takes its 16, 24, 32 and 48px layers from the small
// drawing and its 64 and 256px layers from the large one, and 32x32.png comes from the small one.
// The exe embeds icon.ico when it is built: see AGENTS.md, "Build and test".

import { spawnSync } from "node:child_process";
import { copyFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const ICONS = "src-tauri/icons";
const SMALL = new Set([16, 24, 32, 48]);
const KEEP = ["128x128.png", "128x128@2x.png", "64x64.png", "icon.icns", "icon.png"];

function render(svg) {
  const out = mkdtempSync(join(tmpdir(), "nitely-icon-"));
  const r = spawnSync(process.platform === "win32" ? "npx.cmd" : "npx", ["tauri", "icon", join(ICONS, svg), "-o", out], { stdio: "inherit", shell: process.platform === "win32" });
  if (r.status !== 0) process.exit(r.status ?? 1);
  return out;
}

/** The layers of an .ico: each one's 16-byte directory entry and its image bytes, by width. */
function layers(file) {
  const buf = readFileSync(file);
  const out = new Map();
  for (let i = 0; i < buf.readUInt16LE(4); i++) {
    const entry = buf.subarray(6 + i * 16, 6 + i * 16 + 16);
    const width = entry[0] || 256;
    const data = buf.subarray(entry.readUInt32LE(12), entry.readUInt32LE(12) + entry.readUInt32LE(8));
    out.set(width, { entry, data });
  }
  return out;
}

function writeIco(file, chosen) {
  const head = Buffer.alloc(6 + chosen.length * 16);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(chosen.length, 4);
  let offset = head.length;
  chosen.forEach(({ entry, data }, i) => {
    const e = Buffer.from(entry);
    e.writeUInt32LE(data.length, 8);
    e.writeUInt32LE(offset, 12);
    e.copy(head, 6 + i * 16);
    offset += data.length;
  });
  writeFileSync(file, Buffer.concat([head, ...chosen.map((l) => l.data)]));
}

const large = render("icon.svg");
const small = render("icon-small.svg");
try {
  const big = layers(join(large, "icon.ico"));
  const little = layers(join(small, "icon.ico"));
  const chosen = [...big.keys()].sort((a, b) => a - b).map((w) => (SMALL.has(w) ? little.get(w) : big.get(w)));
  if (chosen.some((l) => !l)) throw new Error("tauri icon did not make every layer");
  writeIco(join(ICONS, "icon.ico"), chosen);
  for (const f of KEEP) copyFileSync(join(large, f), join(ICONS, f));
  copyFileSync(join(small, "32x32.png"), join(ICONS, "32x32.png"));
  console.log(`icon.ico: ${[...big.keys()].sort((a, b) => a - b).map((w) => `${w}px from ${SMALL.has(w) ? "icon-small.svg" : "icon.svg"}`).join(", ")}`);
} finally {
  rmSync(large, { recursive: true, force: true });
  rmSync(small, { recursive: true, force: true });
}
