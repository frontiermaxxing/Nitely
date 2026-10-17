// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { decide, promoted } from "./release-meta.mjs";

test("tags decide the channel, the version and how the release is published", () => {
  assert.deepEqual(
    [decide("v0.2.0"), decide("v0.2.0-beta.3"), decide("v0.2.0-test.1")].map((r) => [r.channel, r.version, r.prerelease, r.draft, r.manifests.join(",")]),
    [
      ["stable", "0.2.0", false, false, "stable,beta"],
      ["beta", "0.2.0-beta.3", true, false, "beta"],
      ["test", "0.2.0-test.1", true, true, ""],
    ],
  );
  for (const bad of ["0.2.0", "v0.2", "v0.2.0-rc.1", "v0.2.0-beta", "nightly-1"]) assert.throws(() => decide(bad), /not a release tag/);
});

test("a nightly carries main's next version, the day and the commit", () => {
  const r = decide("nightly", { base: "0.3.0", date: new Date("2026-10-16T05:00:00Z"), sha: "0123456789abcdef" });
  assert.equal(r.version, "0.3.0-nightly.20261016.g0123456");
  assert.equal(r.tag, "nightly");
  assert.deepEqual(r.manifests, ["nightly"]);
  assert.throws(() => decide("nightly", {}), /version main carries/);
});

test("versions sort the way the updater compares them", () => {
  // Semver pre-releases: beta < nightly (later letters), both below the release itself.
  const order = ["0.2.0-beta.1", "0.2.0-beta.2", "0.2.0-nightly.20261016.g1a2b3c4", "0.2.0-nightly.20261017.g0aa", "0.2.0", "0.3.0-beta.1"];
  // The comparison semver.org section 11 defines, as the updater's semver crate does.
  const ids = (s) => s.split(".").map((p) => (/^\d+$/.test(p) ? Number(p) : p));
  const cmpIds = (a, b) => {
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      if (a[i] === undefined) return -1;
      if (b[i] === undefined) return 1;
      if (a[i] === b[i]) continue;
      const [na, nb] = [typeof a[i] === "number", typeof b[i] === "number"];
      if (na !== nb) return na ? -1 : 1;
      return a[i] < b[i] ? -1 : 1;
    }
    return 0;
  };
  const cmp = (x, y) => {
    const [cx, px] = x.split(/-(.*)/s);
    const [cy, py] = y.split(/-(.*)/s);
    const core = cmpIds(ids(cx), ids(cy));
    if (core !== 0) return core;
    if (px === undefined || py === undefined) return px === py ? 0 : px === undefined ? 1 : -1;
    return cmpIds(ids(px), ids(py));
  };
  assert.deepEqual([...order].reverse().sort(cmp), order);
});

test("promoting a beta names its stable tag", () => {
  assert.equal(promoted("v0.2.0-beta.3"), "v0.2.0");
  assert.throws(() => promoted("v0.2.0"), /not a beta tag/);
});
