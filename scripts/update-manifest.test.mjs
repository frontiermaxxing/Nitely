// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { manifest } from "./update-manifest.mjs";

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
