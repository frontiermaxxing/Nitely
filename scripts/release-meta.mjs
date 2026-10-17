// Which channel a release build belongs to, and the version it is stamped with. One place decides
// it, for the release workflow and its tests:
//
//   tag v0.2.0           stable    0.2.0                       a full release
//   tag v0.2.0-beta.1    beta      0.2.0-beta.1                a pre-release
//   tag v0.2.0-test.1    test      0.2.0-test.1                a draft nobody else sees, no update manifest
//   nightly (from main)  nightly   0.2.0-nightly.20261016.1234.g1a2b3c4: main's version, the day,
//                                 main's commit count (so a later build that day sorts later) and the commit
//
// `node scripts/release-meta.mjs <tag|nightly> [sha] [count]` prints the decision as KEY=value
// lines, for $GITHUB_OUTPUT; `compare A B` prints -1, 0 or 1.

import { pathToFileURL } from "node:url";
import { current } from "./version.mjs";

const TAG = /^v(\d+\.\d+\.\d+)(?:-(beta|test)\.(\d+))?$/;

/**
 * The release for `ref` ("nightly", or a tag like v0.2.0-beta.1). `base` is the version main
 * carries (the next release), `date` a Date, `sha` the commit and `count` how many commits main
 * has (it only grows, so two nightlies on one day sort in the order they were built).
 */
export function decide(ref, { base, date = new Date(), sha = "", count = 0 } = {}) {
  if (ref === "nightly") {
    if (!base) throw new Error("a nightly needs the version main carries");
    if (!(Number(count) > 0)) throw new Error("a nightly needs main's commit count");
    const core = base.replace(/-.*$/, "");
    const day = date.toISOString().slice(0, 10).replaceAll("-", "");
    // A short sha can be all digits with a leading zero, which semver does not allow; the g keeps
    // it an identifier, as git describe does. It only names the commit: the count orders.
    const version = `${core}-nightly.${day}.${Number(count)}.g${sha.slice(0, 7)}`;
    return { channel: "nightly", version, tag: "nightly", prerelease: true, draft: false, manifests: ["nightly"], title: `Nitely ${version} (nightly)` };
  }
  const m = TAG.exec(ref);
  if (!m) throw new Error(`${ref} is not a release tag: use v1.2.3, v1.2.3-beta.N or v1.2.3-test.N`);
  const [, core, kind, n] = m;
  const version = kind ? `${core}-${kind}.${n}` : core;
  if (!kind) {
    // Beta users take a stable release too: it is newer than every beta before it.
    return { channel: "stable", version, tag: ref, prerelease: false, draft: false, manifests: ["stable", "beta"], title: `Nitely ${version}` };
  }
  if (kind === "beta") return { channel: "beta", version, tag: ref, prerelease: true, draft: false, manifests: ["beta"], title: `Nitely ${version} (beta)` };
  return { channel: "test", version, tag: ref, prerelease: true, draft: true, manifests: [], title: `Nitely ${version} (test build)` };
}

/** -1, 0 or 1 as semver orders `a` and `b` (semver.org section 11), as the updater compares. */
export function compareVersions(a, b) {
  const ids = (s) => s.split(".").map((p) => (/^\d+$/.test(p) ? Number(p) : p));
  const cmpIds = (x, y) => {
    for (let i = 0; i < Math.max(x.length, y.length); i++) {
      if (x[i] === undefined) return -1;
      if (y[i] === undefined) return 1;
      if (x[i] === y[i]) continue;
      const [nx, ny] = [typeof x[i] === "number", typeof y[i] === "number"];
      if (nx !== ny) return nx ? -1 : 1;
      return x[i] < y[i] ? -1 : 1;
    }
    return 0;
  };
  const [ca, pa] = a.split(/-(.*)/s);
  const [cb, pb] = b.split(/-(.*)/s);
  const core = cmpIds(ids(ca), ids(cb));
  if (core !== 0) return core;
  if (pa === undefined || pb === undefined) return pa === pb ? 0 : pa === undefined ? 1 : -1;
  return cmpIds(ids(pa), ids(pb));
}

/** The stable tag a beta is promoted to: v0.2.0-beta.3 becomes v0.2.0. */
export function promoted(betaTag) {
  const m = TAG.exec(betaTag);
  if (!m || m[2] !== "beta") throw new Error(`${betaTag} is not a beta tag like v0.2.0-beta.1`);
  return `v${m[1]}`;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const [ref, sha = "", count = "0"] = process.argv.slice(2);
  try {
    if (ref === "compare") {
      console.log(compareVersions(sha, count));
      process.exit(0);
    }
    const r = ref === "promote" ? { stable: promoted(sha) } : decide(ref, { base: current(), sha, count: Number(count) });
    for (const [k, v] of Object.entries(r)) console.log(`${k}=${Array.isArray(v) ? v.join(" ") : v}`);
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
