// `npm test`: compiles the frontend's *.test.ts files with tsc into .test-build, then runs them and
// the scripts' own scripts/*.test.mjs with node:test. Compiling first keeps it working on Node 20,
// which cannot run .ts files; the files are listed by hand because Node 20 takes no globs and newer
// Node no longer reads folders.
//
// `node scripts/test.mjs <files>` runs only those test files (src/**/*.test.ts or scripts/*.test.mjs),
// with the TAP reporter, so Nitely reads which tests ran (NIT-750, the map's `files_command`).

import { existsSync, readdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const OUT = ".test-build";

function run(args) {
  const r = spawnSync(process.execPath, args, { stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status ?? 1);
}

function find(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? find(join(dir, e.name)) : /\.test\.m?js$/.test(e.name) ? [join(dir, e.name)] : [],
  );
}

/** Where a selected test file runs from: a .ts test as tsc compiled it, anything else as it is. */
function compiled(file) {
  const path = file.replace(/\\/g, "/");
  const out = /\.tsx?$/.test(path) ? join(OUT, path.replace(/\.tsx?$/, ".js")) : path;
  if (!existsSync(out)) {
    console.error(`no test file at ${out} for ${file}`);
    process.exit(1);
  }
  return out;
}

const selected = process.argv.slice(2);
rmSync(OUT, { recursive: true, force: true });
if (selected.length === 0 || selected.some((f) => /\.tsx?$/.test(f))) {
  run(["node_modules/typescript/bin/tsc", "-p", "tsconfig.test.json"]);
}
// The frontend's compiled tests, and the tests of the repo's own scripts (scripts/*.test.mjs).
// In a build lane, as many test files at once as the lane's share of cores (NITELY_JOBS), not one
// per core of the machine (NIT-266).
const jobs = Number(process.env.NITELY_JOBS);
const concurrency = Number.isInteger(jobs) && jobs > 0 ? [`--test-concurrency=${jobs}`] : [];
const files = selected.length > 0 ? selected.map(compiled) : [...find(OUT), ...find("scripts")];
const reporter = selected.length > 0 ? ["--test-reporter=tap"] : [];
run(["--enable-source-maps", "--test", ...reporter, ...concurrency, ...files]);
