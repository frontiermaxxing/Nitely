#!/usr/bin/env node
// Execute this adapter from a trusted integration checkout, outside the candidate directory.
// It deliberately does not upload, publish, promote, create tags, or rebuild release artifacts.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, realpathSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

export const REQUIRED = [
  { workflow: ".github/workflows/ci.yml", jobs: ["Gate (windows-latest)", "Gate (macos-latest)", "Gate (ubuntu-latest)"] },
  { workflow: ".github/workflows/app.yml", jobs: ["Inbox in the real app (windows-latest)"] },
  { workflow: ".github/workflows/release-candidate.yml", jobs: ["Installer and upgrade (windows-latest)"], artifacts: true },
];

function invoke(program, args, options = {}) {
  const result = spawnSync(program, args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024, ...options });
  if (result.error || result.status !== 0) throw new Error(`${program} failed: ${result.error?.message ?? result.stderr?.trim() ?? result.status}`);
  return result.stdout;
}

function pages(value, key) {
  return (Array.isArray(value) ? value : [value]).flatMap(page => page[key] ?? []);
}

// The API client is injectable for tests only. The executable always calls authenticated gh api;
// it never accepts a local JSON "CI passed" file or caller-supplied job names.
export function verifyActions({ identity, repository, archiveDigest, api }) {
  if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) throw new Error("expected owner/repository");
  if (!/^[a-f0-9]{40,64}$/.test(identity.candidate)) throw new Error("verifier returned no exact candidate commit");
  const runs = pages(api(`repos/${repository}/actions/runs?head_sha=${identity.candidate}&per_page=100`), "workflow_runs");
  const verified = [];
  for (const required of REQUIRED) {
    // Include pending/failed newer attempts. An old green run never hides their state.
    const run = runs.filter(run => run.path === required.workflow && run.head_sha === identity.candidate
      && ["push", "workflow_dispatch"].includes(run.event)
      && run.head_repository?.full_name === repository)
      .sort((a, b) => b.id - a.id || b.run_attempt - a.run_attempt)[0];
    if (!run || run.status !== "completed" || run.conclusion !== "success") {
      throw new Error(`${required.workflow} has no latest successful run for the exact stamped candidate`);
    }
    if (!Number.isSafeInteger(run.run_attempt) || run.run_attempt < 1) throw new Error("workflow run has no valid attempt identity");
    const jobs = pages(api(`repos/${repository}/actions/runs/${run.id}/attempts/${run.run_attempt}/jobs?per_page=100`), "jobs");
    for (const name of required.jobs) {
      const job = jobs.find(job => job.name === name);
      if (!job || job.head_sha !== identity.candidate || job.status !== "completed" || job.conclusion !== "success") {
        throw new Error(`${name} did not pass for the exact stamped candidate and latest attempt`);
      }
    }
    let artifact;
    if (required.artifacts) {
      const artifacts = pages(api(`repos/${repository}/actions/runs/${run.id}/artifacts?per_page=100`), "artifacts");
      artifact = artifacts.filter(item => item.name === `release-candidate-${identity.candidate}` && !item.expired
        && Date.parse(item.created_at) >= Date.parse(run.run_started_at))
        .sort((a, b) => b.id - a.id)[0];
      if (!artifact || !archiveDigest || artifact.digest !== `sha256:${archiveDigest}`) {
        throw new Error("publication archive is not the exact artifact retained by the passing installer/upgrade run");
      }
    }
    verified.push({ workflow: required.workflow, run: run.id, attempt: run.run_attempt, artifact: artifact?.id });
  }
  return verified;
}

function outside(candidate, path, label) {
  const rel = relative(candidate, realpathSync(path));
  if (!rel || (!isAbsolute(rel) && rel !== ".." && !rel.startsWith(`..${sep}`))) {
    throw new Error(`${label} must be outside the candidate checkout`);
  }
}

export async function main(argv = process.argv.slice(2)) {
  const { values, positionals } = parseArgs({ args: argv, allowPositionals: true, options: {
    verifier: { type: "string" }, worktree: { type: "string" }, source: { type: "string" },
    "author-vendor": { type: "string" }, repository: { type: "string" }, "artifact-archive": { type: "string" },
  } });
  const phase = positionals[0];
  if (positionals.length !== 1 || !["gate", "review", "check"].includes(phase)
      || !values.verifier || !values.worktree || !values.source || !values["author-vendor"]) {
    throw new Error("usage: release-verify-ci.mjs gate|review|check --verifier TRUSTED_BINARY --worktree CANDIDATE --source SOURCE --author-vendor AUTHOR [--repository OWNER/REPO --artifact-archive ZIP]");
  }
  const candidate = realpathSync(values.worktree);
  const verifier = realpathSync(values.verifier);
  outside(candidate, verifier, "trusted verifier");
  outside(candidate, fileURLToPath(import.meta.url), "CI adapter");
  if (!process.env.NITELY_HOME || !isAbsolute(process.env.NITELY_HOME)) throw new Error("set a private absolute NITELY_HOME shared by all phases");
  outside(candidate, process.env.NITELY_HOME, "verification database");
  if (phase === "gate" && Object.entries(process.env).some(([key, value]) => value && /(?:^|_)(OPENAI_API_KEY|ANTHROPIC_API_KEY|CODEX_API_KEY|CLAUDE_CODE_OAUTH_TOKEN)$/.test(key))) {
    throw new Error("reviewer credentials must be absent from gate/build commands; supply them only in the separate review step");
  }
  const identity = JSON.parse(invoke(verifier, ["release-verify", phase, "--worktree", candidate,
    "--integration-ref", "refs/remotes/origin/main", "--source", values.source, "--author-vendor", values["author-vendor"]]));
  if (phase === "check") {
    if (!values.repository || !values["artifact-archive"]) throw new Error("publication/promotion check requires --repository and the downloaded --artifact-archive");
    const archive = resolve(values["artifact-archive"]);
    const hasher = createHash("sha256");
    for await (const chunk of createReadStream(archive)) hasher.update(chunk);
    const archiveDigest = hasher.digest("hex");
    const checks = verifyActions({ identity, repository: values.repository, archiveDigest,
      api: endpoint => JSON.parse(invoke("gh", ["api", "--paginate", "--slurp", endpoint])) });
    const fresh = JSON.parse(invoke(verifier, ["release-verify", "check", "--worktree", candidate,
      "--integration-ref", "refs/remotes/origin/main", "--source", values.source, "--author-vendor", values["author-vendor"]]));
    if (JSON.stringify(fresh) !== JSON.stringify(identity)) throw new Error("release identity changed while checking CI");
    console.log(JSON.stringify({ ...identity, checks, archive, archive_sha256: archiveDigest }));
  } else console.log(JSON.stringify(identity));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
