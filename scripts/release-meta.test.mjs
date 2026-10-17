// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { compareVersions, decide, promoted } from "./release-meta.mjs";

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
  const r = decide("nightly", { base: "0.3.0", date: new Date("2026-10-16T05:00:00Z"), sha: "0123456789abcdef", count: 1234 });
  assert.equal(r.version, "0.3.0-nightly.20261016.1234.g0123456");
  assert.equal(r.tag, "nightly");
  assert.deepEqual(r.manifests, ["nightly"]);
  assert.throws(() => decide("nightly", {}), /version main carries/);
  assert.throws(() => decide("nightly", { base: "0.3.0" }), /commit count/);
  // Two builds on one day: the later one has more commits, whatever its hash.
  const day = { base: "0.3.0", date: new Date("2026-10-16T05:00:00Z") };
  const [early, late] = [decide("nightly", { ...day, sha: "ffff", count: 100 }), decide("nightly", { ...day, sha: "0000", count: 101 })];
  assert.equal(compareVersions(early.version, late.version), -1);
});

test("versions sort the way the updater compares them", () => {
  // Semver pre-releases: beta < nightly (later letters), both below the release itself.
  const order = ["0.2.0-beta.1", "0.2.0-beta.2", "0.2.0-nightly.20261016.99.g1a2b3c4", "0.2.0-nightly.20261016.100.g0aa", "0.2.0", "0.3.0-beta.1"];
  assert.deepEqual([...order].reverse().sort(compareVersions), order);
});

test("promoting a beta names its stable tag", () => {
  assert.equal(promoted("v0.2.0-beta.3"), "v0.2.0");
  assert.throws(() => promoted("v0.2.0"), /not a beta tag/);
});
