import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { latestRun, requireCandidateWorkflows, requireIdentity, validateMetadata, verifyFiles } from "./verified-release.mjs";

const meta = () => ({
  source: "a".repeat(40), candidate: "b".repeat(40), policy: "c".repeat(40),
  tag: "v1.2.3", requested: "v1.2.3-beta.2", promote: true, channel: "stable",
  version: "1.2.3", prerelease: false, draft: false, manifests: ["stable", "beta"],
  date: "2026-10-20T00:00:00Z", count: 12, baseline: "v1.2.2", branch: "nitely-release-candidate/123-1",
});
test("release identity rejects a mismatched promotion, channel, or unscoped temporary ref", () => {
  assert.equal(validateMetadata(meta()).tag, "v1.2.3");
  for (const change of [{ requested: "v9.0.0-beta.2" }, { channel: "beta" }, { manifests: ["nightly"] },
    { branch: "main" }, { candidate: "" }, { baseline: "latest" }]) {
    assert.throws(() => validateMetadata({ ...meta(), ...change }));
  }
});
test("new pending/failed CI attempts supersede earlier green checks for the exact candidate", () => {
  const base = { path: ".github/workflows/ci.yml", head_sha: meta().candidate,
    head_repository: { full_name: "owner/repo" }, event: "workflow_dispatch" };
  const runs = [{ ...base, id: 3, run_attempt: 1, conclusion: "success" },
    { ...base, id: 3, run_attempt: 2, status: "queued" },
    { ...base, id: 4, head_sha: meta().source, conclusion: "success" },
    { ...base, id: 5, head_repository: { full_name: "fork/repo" }, conclusion: "success" }];
  assert.equal(latestRun(runs, base.path, meta().candidate, "owner/repo").run_attempt, 2);
});
test("old release sources fail before dispatch rather than pretending stamped CI exists", () => {
  assert.throws(() => requireCandidateWorkflows(() => "on:\n  push:\n"), /predates/);
  assert.throws(() => requireCandidateWorkflows(() => { throw new Error("missing"); }), /missing/);
  requireCandidateWorkflows(path => readFileSync(new URL(`../${path}`, import.meta.url), "utf8"));
});
test("publication authorization must match the prepared source, candidate and policy", () => {
  assert.equal(requireIdentity(meta(), meta()).candidate, meta().candidate);
  for (const key of ["source", "candidate", "policy"]) {
    assert.throws(() => requireIdentity({ ...meta(), [key]: "d".repeat(40) }, meta()), new RegExp(key));
  }
});
test("publication requires complete hashes, original identity, and only tested release files", () => {
  const dir = mkdtempSync(join(tmpdir(), "nitely-release-files-"));
  try {
    const data = meta();
    const files = {
      "Nitely_1.2.3_x64-setup.exe": "installer",
      "nitely_1.2.3_x86_64-pc-windows-msvc.exe": "cli",
      "candidate.json": JSON.stringify({ candidate: data.candidate, source: data.source, version: data.version, channel: data.channel }),
    };
    for (const [name, content] of Object.entries(files)) writeFileSync(join(dir, name), content);
    const sums = Object.entries(files).map(([name, content]) => `${createHash("sha256").update(content).digest("hex")}  ${name}`).join("\n");
    writeFileSync(join(dir, "SHA256SUMS.txt"), sums);
    assert.equal(verifyFiles(dir, data).length, 4);
    assert.throws(() => verifyFiles(dir, { ...data, source: "d".repeat(40) }), /source/);
    writeFileSync(join(dir, "Nitely_1.2.3_x64-setup.exe"), "rebuilt");
    assert.throws(() => verifyFiles(dir, data), /checksum/);
    writeFileSync(join(dir, "Nitely_1.2.3_x64-setup.exe"), "installer");
    writeFileSync(join(dir, "SHA256SUMS.txt"), sums + "\n" + sums.split("\n")[0]);
    assert.throws(() => verifyFiles(dir, data), /duplicate/);
    writeFileSync(join(dir, "SHA256SUMS.txt"), sums);
    writeFileSync(join(dir, "extra.exe"), "unverified");
    assert.throws(() => verifyFiles(dir, data), /unexpected/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("opt-in routes publication and promotion through isolated exact-candidate jobs", () => {
  const read = name => readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), "utf8");
  for (const name of ["release", "promote"]) {
    assert.match(read(name), /if: vars.NITELY_VERIFIED_RELEASES == 'true'/);
    assert.match(read(name), /if: vars.NITELY_VERIFIED_RELEASES != 'true'/);
    assert.match(read(name), /gh workflow run verified-release.yml.*--ref main/);
  }
  const workflow = read("verified-release");
  const gate = workflow.slice(workflow.indexOf("\n  full:"), workflow.indexOf("\n  review:"));
  assert.doesNotMatch(gate, /secrets\.|CODEX_HOME|login --with-api-key/);
  const publish = workflow.slice(workflow.indexOf("\n  publish:"), workflow.indexOf("\n  cleanup:"));
  assert.doesNotMatch(publish, /npm ci|cargo build|secrets\.|target\/release\/nitely.exe release/);
  assert.match(publish, /release-verify-ci.mjs check/);
  assert.match(publish, /release-archive.py candidate.zip out/);
  assert.match(workflow, /release-reviewed-receipt/);
});
