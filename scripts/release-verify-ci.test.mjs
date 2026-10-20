import test from "node:test";
import assert from "node:assert/strict";
import { REQUIRED, verifyActions } from "./release-verify-ci.mjs";

const candidate = "a".repeat(40);
const source = "b".repeat(40);
const digest = "c".repeat(64);
function fixture() {
  const runs = REQUIRED.map((required, i) => ({ id: i + 1, path: required.workflow, head_sha: candidate, event: "push",
    head_repository: { full_name: "example/app" }, status: "completed", conclusion: "success", run_attempt: 2, run_started_at: "2026-01-01T00:00:00Z" }));
  const jobs = REQUIRED.map(required => required.jobs.map(name => ({ name, head_sha: candidate,
    status: "completed", conclusion: "success", run_attempt: 2 })));
  const artifacts = [{ id: 4, name: `release-candidate-${candidate}`, expired: false, digest: `sha256:${digest}`, created_at: "2026-01-01T01:00:00Z" }];
  const api = endpoint => {
    if (endpoint.includes("/actions/runs?")) return [{ workflow_runs: runs }];
    const id = Number(endpoint.match(/\/runs\/(\d+)/)?.[1]);
    if (endpoint.includes("/jobs?")) {
      assert.ok(endpoint.includes("/attempts/2/jobs?"), "query the specific run attempt, not an older job list");
      return [{ jobs: jobs[id - 1] }];
    }
    return [{ artifacts }];
  };
  return { runs, jobs, artifacts, check: () => verifyActions({ identity: { candidate, source }, repository: "example/app", archiveDigest: digest, api }) };
}

test("requires all OS/native checks and the installer-tested archive on the stamped candidate", () => {
  const f = fixture();
  assert.equal(f.check().length, 3);
  f.jobs[0][1].conclusion = "skipped";
  assert.throws(f.check, /macos/);
});
test("source CI cannot certify the stamped candidate", () => {
  const f = fixture();
  f.runs[0].head_sha = source;
  assert.throws(f.check, /exact stamped/);
});
test("pull request merge-ref CI cannot certify an exact candidate", () => {
  const f = fixture();
  f.runs[0].event = "pull_request";
  assert.throws(f.check, /exact stamped/);
});
test("failed or pending reruns supersede old green evidence", () => {
  const f = fixture();
  f.runs.push({ ...f.runs[0], id: 99, status: "in_progress", conclusion: null });
  assert.throws(f.check, /latest successful/);
  f.runs[3].status = "completed";
  f.runs[3].conclusion = "failure";
  assert.throws(f.check, /latest successful/);
});
test("wrong attempt, missing native coverage, and a changed publication archive fail closed", () => {
  const f = fixture();
  f.jobs[1][0].head_sha = source;
  assert.throws(f.check, /latest attempt/);
  f.jobs[1][0].head_sha = candidate;
  f.artifacts[0].digest = `sha256:${"d".repeat(64)}`;
  assert.throws(f.check, /exact artifact/);
  f.artifacts[0].digest = `sha256:${digest}`;
  f.runs.pop();
  assert.throws(f.check, /release-candidate/);
});
