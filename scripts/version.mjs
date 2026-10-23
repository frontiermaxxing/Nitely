// Nitely's version, kept in five files: the Cargo workspace (Cargo.toml, and the workspace crates
// in Cargo.lock), package.json and package-lock.json, and src-tauri/tauri.conf.json.
//
//   node scripts/version.mjs              prints it, and fails when the files disagree
//   node scripts/version.mjs set 0.2.0    writes it to all five
//
// The release workflow stamps the version it builds (0.2.0-nightly.20261016.1a2b3c4, 0.2.0-beta.1,
// 0.2.0) without committing it; main keeps the next version to release.

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;
const CRATES = ["nitely-app", "nitely-cli", "nitely-core", "nitely-crew", "nitely-leaderboard"];

/** Each file's version, read with the same patterns `set` writes. */
export function read(root = ".") {
  const text = (f) => readFileSync(join(root, f), "utf8");
  const cargo = text("Cargo.toml").match(/\[workspace\.package\][^[]*?\nversion\s*=\s*"([^"]+)"/)?.[1];
  const lock = text("Cargo.lock");
  const crates = CRATES.map((name) => lock.match(new RegExp(`\\[\\[package\\]\\]\\r?\\nname = "${name}"\\r?\\nversion = "([^"]+)"`))?.[1]);
  const pkgLock = JSON.parse(text("package-lock.json"));
  return {
    "Cargo.toml": cargo,
    ...Object.fromEntries(CRATES.map((name, i) => [`Cargo.lock ${name}`, crates[i]])),
    "package.json": JSON.parse(text("package.json")).version,
    "package-lock.json": pkgLock.version,
    'package-lock.json packages[""]': pkgLock.packages?.[""]?.version,
    "src-tauri/tauri.conf.json": JSON.parse(text("src-tauri/tauri.conf.json")).version,
  };
}

/** The one version all files agree on; throws naming the files that differ. */
export function current(root = ".") {
  const all = read(root);
  const versions = new Set(Object.values(all));
  if (versions.size !== 1 || versions.has(undefined)) {
    throw new Error(`the version files disagree:\n${Object.entries(all).map(([f, v]) => `  ${f}: ${v ?? "(not found)"}`).join("\n")}`);
  }
  return [...versions][0];
}

/** Writes `version` to every file, changing nothing else in them. */
export function set(version, root = ".") {
  if (!SEMVER.test(version)) throw new Error(`${version} is not a version like 0.2.0 or 0.2.0-beta.1`);
  const edit = (f, change) => {
    const p = join(root, f);
    const before = readFileSync(p, "utf8");
    const after = change(before);
    if (after === before && !before.includes(version)) throw new Error(`found no version to change in ${f}`);
    writeFileSync(p, after);
  };
  edit("Cargo.toml", (t) => t.replace(/(\[workspace\.package\][^[]*?\nversion\s*=\s*")[^"]+(")/, `$1${version}$2`));
  edit("Cargo.lock", (t) =>
    CRATES.reduce((acc, name) => acc.replace(new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${name}"\\r?\\nversion = ")[^"]+(")`), `$1${version}$2`), t),
  );
  // The first "version" in each JSON file is its own; dependencies come after.
  edit("package.json", (t) => t.replace(/("version":\s*")[^"]+(")/, `$1${version}$2`));
  edit("package-lock.json", (t) => t.replace(/("version":\s*")[^"]+(")/, `$1${version}$2`).replace(/("packages":\s*\{\s*"":\s*\{[^}]*?"version":\s*")[^"]+(")/, `$1${version}$2`));
  edit("src-tauri/tauri.conf.json", (t) => t.replace(/("version":\s*")[^"]+(")/, `$1${version}$2`));
  return current(root);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const [cmd, version] = process.argv.slice(2);
  try {
    if (cmd === "set" && version) console.log(set(version));
    else if (!cmd) console.log(current());
    else throw new Error("usage: node scripts/version.mjs [set VERSION]");
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
