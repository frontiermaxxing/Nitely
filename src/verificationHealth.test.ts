import { test } from "node:test";
import assert from "node:assert/strict";
import { auditedEscapeRate, verificationPresentation, verificationRepoName, verificationRevision } from "./verificationHealth.js";
import type { VerificationHealth } from "./types";

const health: VerificationHealth = { repo: "C:\\Code\\example\\.git", project: "APP", observed_head: "a".repeat(40), verified_head: "a".repeat(40), pending_since: 0, reduced_count: 0, checked_at: 1, status: "green", message: "", recovery_issue: null };

test("only a matching verified revision displays Verified", () => {
  assert.equal(verificationPresentation(health).label, "Verified");
  assert.equal(verificationPresentation({ ...health, observed_head: "b".repeat(40) }).label, "Verification pending");
  assert.equal(verificationPresentation({ ...health, verified_head: null }).label, "Verification pending");
  assert.equal(verificationPresentation({ ...health, status: "unknown" }).label, "Verification unavailable");
});

test("failed, paused and recovering states remain distinct even with matching revisions", () => {
  assert.equal(verificationPresentation({ ...health, status: "failed" }).label, "Verification failed");
  assert.equal(verificationPresentation({ ...health, status: "paused" }).label, "Verification paused");
  assert.equal(verificationPresentation({ ...health, status: "recovering" }).label, "Recovering");
});

test("display identifiers omit machine paths and reject nonrevision content", () => {
  assert.equal(verificationRepoName(health.repo), "example");
  assert.equal(verificationRepoName("/code/example/.git/"), "example");
  assert.equal(verificationRevision(health.observed_head), "aaaaaaa");
  assert.equal(verificationRevision("C:\\Code\\example"), null);
});

test("unaudited outcomes stay unknown instead of appearing as zero escapes", () => {
  const rate = { name: "display-copy", landed: 12, audited: 0, confirmed_escape_lands: 0, declared_lands: 12, matching_declarations: 10, audited_escape_rate: null };
  assert.equal(auditedEscapeRate(rate), "Unknown — no completed audits");
  assert.equal(auditedEscapeRate({ ...rate, audited: 4, audited_escape_rate: 0 }), "0.0% of audited");
  assert.equal(auditedEscapeRate({ ...rate, audited: 4, audited_escape_rate: 0.25 }), "25.0% of audited");
});
