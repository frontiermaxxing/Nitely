// `npm test`: compiles the frontend's *.test.ts files with tsc into .test-build, then runs them and
// the scripts' own scripts/*.test.mjs with node:test. Compiling first keeps it working on Node 20,
// which cannot run .ts files; the files are listed by hand because Node 20 takes no globs and newer
// Node no longer reads folders.

import { readdirSync, rmSync } from "node:fs";
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

rmSync(OUT, { recursive: true, force: true });
run(["node_modules/typescript/bin/tsc", "-p", "tsconfig.test.json"]);
// The frontend's compiled tests, and the tests of the repo's own scripts (scripts/*.test.mjs).
run(["--enable-source-maps", "--test", ...find(OUT), ...find("scripts")]);
