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
    const version = "9.8.7-nightly.20261023.1234.gabcdef0";
    assert.equal(set(version, dir), version);
    const lock = readFileSync(join(dir, "Cargo.lock"), "utf8");
    // Derive the contract from workspace membership, independently of the stamper's list.
    // A new version-inheriting package must not silently break the next locked release build.
    const members = readFileSync("Cargo.toml", "utf8").match(/members\s*=\s*\[([^\]]+)\]/)[1].matchAll(/"([^"]+)"/g);
    const inherited = [...members].map(([, path]) => readFileSync(join(path, "Cargo.toml"), "utf8"))
      .filter((manifest) => /version\.workspace\s*=\s*true/.test(manifest))
      .map((manifest) => manifest.match(/\[package\][\s\S]*?\bname\s*=\s*"([^"]+)"/)[1]);
    const packages = (text) => [...text.matchAll(/\[\[package\]\]\r?\nname = "([^"]+)"\r?\nversion = "([^"]+)"/g)]
      .map(([, name, version]) => ({ name, version }));
    const before = packages(readFileSync("Cargo.lock", "utf8"));
    const after = packages(lock);
    for (const name of inherited) assert.equal(after.find((p) => p.name === name)?.version, version, `${name} inherits the release version`);
    assert.deepEqual(after.filter((p) => !inherited.includes(p.name)), before.filter((p) => !inherited.includes(p.name)), "dependency versions untouched");
    const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    assert.equal(pkg.dependencies.react, JSON.parse(readFileSync("package.json", "utf8")).dependencies.react, "dependencies untouched");
    assert.throws(() => set("not-a-version", dir), /not a version/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
