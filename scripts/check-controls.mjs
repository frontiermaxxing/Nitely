// The design's "no native controls left" check (NIT-439): every select is the shared Select, every
// checkbox the shared Checkbox, and no option offers "Inherit (…)" (the default option carries a
// "default" mark instead). `npm test` runs it through check-controls.test.mjs; run it alone with
// `node scripts/check-controls.mjs` to list what it finds.

import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const RULES = [
  { pattern: /<select[\s>]/, why: "a native <select>: use Select from src/ui" },
  { pattern: /type=\{?["']checkbox["']\}?/, why: "a native checkbox: use Checkbox from src/ui" },
  { pattern: /Inherit \(/, why: "an \"Inherit (…)\" option: mark the default option instead" },
];

// The shared controls themselves draw on the native element underneath.
const OWN = new Set(["src/ui/Toggles.tsx"]);

/** Comments say what the code does not do ("no Inherit (…) entry"), so they never count. */
function code(text) {
  return text.replace(/\/\*[\s\S]*?\*\//g, (c) => c.replace(/[^\n]/g, " ")).replace(/^(\s*)\/\/.*$/gm, "$1");
}

/** What breaks the rules in one file's text, one finding per line. */
export function findingsIn(file, text) {
  if (OWN.has(file)) return [];
  const out = [];
  code(text)
    .split("\n")
    .forEach((line, i) => {
      for (const r of RULES) if (r.pattern.test(line)) out.push({ file, line: i + 1, why: r.why });
    });
  return out;
}

/** Every finding under `dir` (src/ by default), in the frontend's sources; tests may quote them. */
export function findings(root = process.cwd(), dir = "src") {
  const walk = (d) =>
    readdirSync(d, { withFileTypes: true }).flatMap((e) => {
      const p = join(d, e.name);
      if (e.isDirectory()) return walk(p);
      return /\.(tsx|ts)$/.test(e.name) && !/\.test\.ts$/.test(e.name) ? [p] : [];
    });
  return walk(join(root, dir)).flatMap((p) => findingsIn(relative(root, p).split(sep).join("/"), readFileSync(p, "utf8")));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const found = findings();
  for (const f of found) console.log(`${f.file}:${f.line}: ${f.why}`);
  process.exit(found.length ? 1 : 0);
}
