// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { cpSync, mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { current, set } from "./version.mjs";

const FILES = ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "src-tauri/tauri.conf.json"];

test("the repo's version files agree", () => {
  assert.match(current(), /^\d+\.\d+\.\d+/);
});

test("set writes one version to every file and nothing else", () => {
  const dir = mkdtempSync(join(tmpdir(), "nitely-version-"));
  try {
    mkdirSync(join(dir, "src-tauri"));
    for (const f of FILES) cpSync(f, join(dir, f));
    assert.equal(set("9.8.7-beta.3", dir), "9.8.7-beta.3");
    const lock = readFileSync(join(dir, "Cargo.lock"), "utf8");
    assert.equal(lock.match(/version = "9\.8\.7-beta\.3"/g).length, 3, "the three workspace crates, no dependency");
    const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    assert.equal(pkg.dependencies.react, JSON.parse(readFileSync("package.json", "utf8")).dependencies.react, "dependencies untouched");
    assert.throws(() => set("not-a-version", dir), /not a version/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
