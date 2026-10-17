// Which channel a release build belongs to, and the version it is stamped with. One place decides
// it, for the release workflow and its tests:
//
//   tag v0.2.0           stable    0.2.0                       a full release
//   tag v0.2.0-beta.1    beta      0.2.0-beta.1                a pre-release
//   tag v0.2.0-test.1    test      0.2.0-test.1                a draft nobody else sees, no update manifest
//   nightly (from main)  nightly   0.2.0-nightly.20261016.g1a2b3c4, from the version main carries
//
// `node scripts/release-meta.mjs <tag|nightly> [sha]` prints the decision as KEY=value lines, for
// $GITHUB_OUTPUT.

import { pathToFileURL } from "node:url";
import { current } from "./version.mjs";

const TAG = /^v(\d+\.\d+\.\d+)(?:-(beta|test)\.(\d+))?$/;

/**
 * The release for `ref` ("nightly", or a tag like v0.2.0-beta.1). `base` is the version main
 * carries (the next release), `date` a Date, `sha` the commit.
 */
export function decide(ref, { base, date = new Date(), sha = "" } = {}) {
  if (ref === "nightly") {
    if (!base) throw new Error("a nightly needs the version main carries");
    const core = base.replace(/-.*$/, "");
    const day = date.toISOString().slice(0, 10).replaceAll("-", "");
    // A short sha can be all digits with a leading zero, which semver does not allow; the g keeps
    // it an identifier, as git describe does.
    const version = `${core}-nightly.${day}.g${sha.slice(0, 7)}`;
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

/** The stable tag a beta is promoted to: v0.2.0-beta.3 becomes v0.2.0. */
export function promoted(betaTag) {
  const m = TAG.exec(betaTag);
  if (!m || m[2] !== "beta") throw new Error(`${betaTag} is not a beta tag like v0.2.0-beta.1`);
  return `v${m[1]}`;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const [ref, sha = ""] = process.argv.slice(2);
  try {
    const r = ref === "promote" ? { stable: promoted(sha) } : decide(ref, { base: current(), sha });
    for (const [k, v] of Object.entries(r)) console.log(`${k}=${Array.isArray(v) ? v.join(" ") : v}`);
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
