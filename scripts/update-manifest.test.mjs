// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { holds, manifest, newest, replaces, same } from "./update-manifest.mjs";

test("a manifest in the updater's format", () => {
  const m = manifest({ version: "0.2.0", url: "https://example.com/Nitely_0.2.0_x64-setup.exe", signature: "c2ln\n", notes: "## Nitely 0.2.0", date: new Date("2026-10-16T05:00:00.123Z") });
  assert.deepEqual(m, {
    version: "0.2.0",
    notes: "## Nitely 0.2.0",
    pub_date: "2026-10-16T05:00:00Z",
    platforms: { "windows-x86_64": { signature: "c2ln", url: "https://example.com/Nitely_0.2.0_x64-setup.exe" } },
  });
  assert.throws(() => manifest({ version: "0.2.0", url: "u" }), /signature/);
});

test("a manifest never moves a channel back", () => {
  assert.equal(replaces("0.2.0", null), true);
  assert.equal(replaces("0.2.1", { version: "0.2.0" }), true);
  assert.equal(replaces("0.2.0", { version: "0.2.0" }), true, "a rebuild of the same version brings its new signature");
  assert.equal(replaces("0.1.9", { version: "0.2.0" }), false);
  assert.equal(replaces("0.2.0-beta.3", { version: "0.2.0" }), false, "a beta never replaces its stable");
});

test("every build names the newest published release of the channel, whatever its own version", () => {
  const release = (tag, version, { draft = false, sig = true } = {}) => ({
    tag_name: tag,
    draft,
    body: `notes for ${version}`,
    assets: [
      { name: `Nitely_${version}_x64-setup.exe`, browser_download_url: `https://example.com/${tag}/Nitely_${version}_x64-setup.exe` },
      ...(sig ? [{ name: `Nitely_${version}_x64-setup.exe.sig`, browser_download_url: "x" }] : []),
      { name: "SHA256SUMS.txt", browser_download_url: "y" },
    ],
  });
  // Listed in whatever order GitHub gives: 0.3.0 is newest, so a build of 0.2.0 names it too.
  const releases = [
    release("v0.2.0", "0.2.0"),
    release("v0.4.0", "0.4.0", { draft: true }),
    release("v0.3.0", "0.3.0"),
    release("v0.3.1-beta.1", "0.3.1-beta.1"),
    release("v0.3.2", "0.3.2", { sig: false }),
    release("v0.3.0-test.2", "0.3.0-test.2"),
    release("nightly", "0.4.0-nightly.20261017.900.gabc1234"),
  ];
  assert.deepEqual(newest(releases, "stable"), {
    tag: "v0.3.0",
    version: "0.3.0",
    url: "https://example.com/v0.3.0/Nitely_0.3.0_x64-setup.exe",
    sig: "Nitely_0.3.0_x64-setup.exe.sig",
    notes: "notes for 0.3.0",
  }, "not a draft, not one without a signature");
  assert.equal(newest(releases, "beta").version, "0.3.1-beta.1", "beta users take betas and stables");
  assert.equal(newest(releases, "nightly").version, "0.4.0-nightly.20261017.900.gabc1234");
  assert.equal(newest([release("v0.1.0-test.1", "0.1.0-test.1")], "stable"), null);

  // Re-derived later, it is the same manifest whatever its date: nothing to upload again.
  const m = (signature, date) => manifest({ version: "0.3.0", url: "u", signature, date: new Date(date) });
  assert.equal(same(m("s", 1), m("s", 2)), true);
  assert.equal(same(m("s", 1), m("t", 1)), false, "a rebuild's new signature is uploaded");
  assert.equal(same(m("s", 1), null), false);
});

test("a build reading its upload back sees whether an older one replaced it", () => {
  assert.equal(holds("0.2.0", { version: "0.2.0" }), true, "its own upload");
  assert.equal(holds("0.2.0", { version: "0.2.1" }), true, "a newer build's");
  assert.equal(holds("0.2.0", { version: "0.1.9" }), false, "an older build uploaded after it");
  assert.equal(holds("0.2.0", null), false, "gone");
});
