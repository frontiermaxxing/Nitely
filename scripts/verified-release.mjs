// Trusted integration-side orchestration. Never run this copy from the release candidate.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFileSync, existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { decide, promoted } from "./release-meta.mjs";
import { current, set } from "./version.mjs";
import { manifest, replaces } from "./update-manifest.mjs";
import { REQUIRED } from "./release-verify-ci.mjs";

export const STAMPS = ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "src-tauri/tauri.conf.json"];
const sha = value => /^[a-f0-9]{40}$/.test(value);
function run(program, args, options = {}) {
  const result = spawnSync(program, args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024, ...options });
  if (result.error || result.status !== 0) throw new Error(`${program} failed: ${result.error?.message ?? result.stderr ?? result.status}`);
  return result.stdout.trim();
}
function git(cwd, ...args) { return run("git", args, { cwd }); }
function api(endpoint, args = []) { return JSON.parse(run("gh", ["api", endpoint, ...args])); }
const repository = () => {
  const repo = process.env.GITHUB_REPOSITORY;
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo ?? "")) throw new Error("missing repository identity");
  return repo;
};
function output(meta) {
  if (process.env.GITHUB_OUTPUT) for (const [key, value] of Object.entries(meta)) {
    if (typeof value !== "object") appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
  }
}
export function requireCandidateWorkflows(read) {
  for (const required of REQUIRED) {
    let text;
    try { text = read(required.workflow); } catch { throw new Error(`release source predates exact candidate support: missing ${required.workflow}`); }
    if (!text.includes("# nitely-exact-release-v1:") || !/^\s+workflow_dispatch:/m.test(text)) {
      throw new Error(`release source predates exact candidate dispatch support in ${required.workflow}; release a source containing the verified-release integration`);
    }
  }
}
export function validateMetadata(meta) {
  if (![meta.source, meta.candidate, meta.policy].every(sha)) throw new Error("invalid release commit identity");
  if (!/^nitely-release-candidate\/\d+-\d+$/.test(meta.branch)) throw new Error("invalid temporary candidate branch");
  const expected = decide(meta.tag, { base: meta.version, sha: meta.source, count: meta.count, date: new Date(meta.date) });
  for (const field of ["tag", "version", "channel", "prerelease", "draft"]) {
    if (expected[field] !== meta[field]) throw new Error(`release metadata disagrees on ${field}`);
  }
  if (JSON.stringify(expected.manifests) !== JSON.stringify(meta.manifests)) throw new Error("invalid channel destinations");
  if (meta.promote && promoted(meta.requested) !== meta.tag) throw new Error("invalid promotion identity");
  if (!/^v\d+\.\d+\.\d+(?:-beta\.\d+)?$/.test(meta.baseline)) throw new Error("no published upgrade baseline");
  return meta;
}
function prepare(candidate, destination) {
  const requested = process.env.RELEASE_REF;
  const promote = process.env.RELEASE_PROMOTE === "true";
  const tag = promote ? promoted(requested) : requested;
  // Validate before using a ref in Git or writing any outputs.
  if (tag !== "nightly") decide(tag);
  const policy = git(candidate, "rev-parse", "refs/remotes/origin/main");
  if (policy !== process.env.GITHUB_SHA) throw new Error("integration moved before preparation; start a new run on main");
  const source = git(candidate, "rev-parse", "--verify", "--end-of-options",
    tag === "nightly" ? "refs/remotes/origin/main^{commit}" : `refs/tags/${requested}^{commit}`);
  git(candidate, "merge-base", "--is-ancestor", source, policy);
  requireCandidateWorkflows(path => git(candidate, "show", `${source}:${path}`));
  if (promote) {
    const release = api(`repos/${repository()}/releases/tags/${requested}`);
    if (release.draft || !release.prerelease) throw new Error("promote a published beta");
    const existing = spawnSync("git", ["rev-parse", "--verify", `refs/tags/${tag}`], { cwd: candidate });
    if (existing.status === 0) throw new Error("stable tag already exists");
  }
  git(candidate, "checkout", "--detach", source);
  const policyText = git(candidate, "show", `${policy}:.nitely/playbook.toml`);
  if (!/^\[release\]\s*$/m.test(policyText)) throw new Error("approve and configure integration [release] policy before activation");
  const baseline = promote ? requested : git(candidate, "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*",
    "--exclude", "*-test.*", `${source}^`);
  if (!/^v\d+\.\d+\.\d+(?:-beta\.\d+)?$/.test(baseline)) throw new Error("no upgrade baseline");
  api(`repos/${repository()}/releases/tags/${baseline}`);
  const date = new Date().toISOString();
  const count = Number(git(candidate, "rev-list", "--count", source));
  const meta = { ...decide(tag, { base: current(candidate), sha: source, count, date: new Date(date) }),
    requested, promote, policy, source, baseline, date, count,
    branch: `nitely-release-candidate/${process.env.GITHUB_RUN_ID}-${process.env.GITHUB_RUN_ATTEMPT}` };
  set(meta.version, candidate);
  git(candidate, "add", "--", ...STAMPS);
  git(candidate, "-c", "user.name=Nitely release CI", "-c", "user.email=ci@example.com",
    "commit", "--allow-empty", "-m", `Stamp ${meta.version} for release verification`);
  meta.candidate = git(candidate, "rev-parse", "HEAD");
  validateMetadata(meta);
  // The object is new: push it with a process-local credential, never a persisted config entry.
  if (!process.env.GH_TOKEN) throw new Error("candidate publication needs the workflow token");
  const credential = Buffer.from(`x-access-token:${process.env.GH_TOKEN}`).toString("base64");
  run("git", ["push", "origin", `HEAD:refs/heads/${meta.branch}`], { cwd: candidate,
    env: { ...process.env, GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "http.https://github.com/.extraheader", GIT_CONFIG_VALUE_0: `AUTHORIZATION: basic ${credential}` } });
  writeFileSync(destination, JSON.stringify(meta, null, 2));
  output(meta);
}

export function latestRun(runs, workflow, candidate, repo) {
  return runs.filter(r => r.path === workflow && r.head_sha === candidate
    && r.head_repository?.full_name === repo && r.event === "workflow_dispatch")
    .sort((a, b) => b.id - a.id || b.run_attempt - a.run_attempt)[0];
}
async function waitForChecks(meta, archive) {
  const repo = repository();
  for (let attempt = 0; attempt < 180; attempt++) {
    const pages = JSON.parse(run("gh", ["api", "--paginate", "--slurp", `repos/${repo}/actions/runs?head_sha=${meta.candidate}&per_page=100`]));
    const runs = pages.flatMap(p => p.workflow_runs);
    const latest = REQUIRED.map(r => latestRun(runs, r.workflow, meta.candidate, repo));
    const failed = latest.find(r => r?.status === "completed" && r.conclusion !== "success");
    if (failed) throw new Error(`required release check failed: ${failed.html_url}`);
    if (latest.every(r => r?.status === "completed" && r.conclusion === "success")) {
      const installer = latest[2];
      const artifacts = api(`repos/${repo}/actions/runs/${installer.id}/artifacts?per_page=100`).artifacts;
      const artifact = artifacts.filter(a => a.name === `release-candidate-${meta.candidate}` && !a.expired
        && Date.parse(a.created_at) >= Date.parse(installer.run_started_at)).sort((a, b) => b.id - a.id)[0];
      if (!artifact) throw new Error("installer check retained no current candidate archive");
      // gh follows the authenticated redirect without forwarding credentials to the blob host.
      const zip = spawnSync("gh", ["api", `repos/${repo}/actions/artifacts/${artifact.id}/zip`], { maxBuffer: 1024 * 1024 * 1024 });
      if (zip.status !== 0) throw new Error("could not download the candidate archive");
      writeFileSync(archive, zip.stdout);
      return;
    }
    await new Promise(resolve => setTimeout(resolve, 30000));
  }
  throw new Error("exact candidate checks did not finish within 90 minutes");
}
export function verifyFiles(dir, meta) {
  const names = readdirSync(dir).sort();
  const required = [`Nitely_${meta.version}_x64-setup.exe`, `nitely_${meta.version}_x86_64-pc-windows-msvc.exe`, "candidate.json", "SHA256SUMS.txt"];
  const allowed = [...required, `${required[0]}.sig`];
  if (required.some(n => !names.includes(n)) || names.some(n => !allowed.includes(n))) throw new Error("unexpected or missing release files");
  const built = JSON.parse(readFileSync(join(dir, "candidate.json"), "utf8"));
  for (const key of ["candidate", "source", "channel", "version"]) if (built[key] !== meta[key]) throw new Error(`artifact ${key} differs from release identity`);
  const entries = readFileSync(join(dir, "SHA256SUMS.txt"), "utf8").trim().split(/\r?\n/);
  const sums = new Map();
  for (const line of entries) {
    const match = /^([a-f0-9]{64}) [ *]([^/\\]+)$/.exec(line);
    if (!match || sums.has(match[2])) throw new Error("invalid or duplicate checksum");
    sums.set(match[2], match[1]);
  }
  if (sums.size !== names.length - 1) throw new Error("checksum inventory differs from archive");
  for (const name of names.filter(n => n !== "SHA256SUMS.txt")) {
    if (sums.get(name) !== createHash("sha256").update(readFileSync(join(dir, name))).digest("hex")) throw new Error(`checksum mismatch: ${name}`);
  }
  return names;
}
export function requireIdentity(identity, meta) {
  for (const key of ["candidate", "source", "policy"]) {
    if (identity[key] !== meta[key]) throw new Error(`authorization ${key} differs from prepared release`);
  }
  return identity;
}
function authorization(meta, archive, trusted, candidate, verifier) {
  git(candidate, "fetch", "origin", "main", "--tags", "--force");
  if (git(candidate, "rev-parse", "refs/remotes/origin/main") !== meta.policy) throw new Error("integration moved: restart the release verification");
  const receipt = run(process.execPath, [join(trusted, "scripts/release-verify-ci.mjs"), "check", "--verifier", verifier,
    "--worktree", candidate, "--source", meta.source, "--author-vendor", process.env.RELEASE_AUTHOR_VENDOR,
    "--repository", repository(), "--artifact-archive", archive]);
  requireIdentity(JSON.parse(receipt), meta);
  return receipt;
}
function publish(meta, archive, dir, trusted, candidate, verifier) {
  const files = verifyFiles(dir, meta);
  const check = () => authorization(meta, archive, trusted, candidate, verifier);
  const identity = check();
  writeFileSync(join(dir, "verification.json"), identity);
  const repo = repository();
  if (meta.promote) {
    // First externally visible promotion mutation, only after exact checks and peer review.
    api(`repos/${repo}/git/refs`, ["-X", "POST", "-f", `ref=refs/tags/${meta.tag}`, "-f", `sha=${meta.source}`]);
  } else if (meta.tag !== "nightly") {
    if (git(candidate, "rev-parse", `refs/tags/${meta.tag}^{commit}`) !== meta.source) throw new Error("source tag moved");
  }
  const notes = `Verified release ${meta.version}.\n\nSource: ${meta.source}\nStamped candidate: ${meta.candidate}\n\n`
    + "The attached verification.json identifies the Full gate, peer review and exact candidate CI. "
    + "Download the Windows setup executable to install. SHA256SUMS.txt lists the tested files' hashes.\n";
  const notesPath = join(resolve(dir, ".."), "release-notes.md");
  writeFileSync(notesPath, notes);
  check();
  if (meta.tag === "nightly" && spawnSync("gh", ["release", "view", "nightly", "--repo", repo]).status === 0) {
    run("gh", ["release", "delete", "nightly", "--repo", repo, "--yes", "--cleanup-tag"]);
  }
  const flags = [...(meta.prerelease ? ["--prerelease"] : []), ...(meta.draft ? ["--draft"] : []),
    ...(meta.tag === "nightly" ? ["--target", meta.source] : ["--verify-tag"])];
  run("gh", ["release", "create", meta.tag, ...files.map(n => join(dir, n)), join(dir, "verification.json"),
    "--repo", repo, "--title", `Nitely ${meta.version}`, "--notes-file", notesPath, ...flags]);
  const signature = join(dir, `Nitely_${meta.version}_x64-setup.exe.sig`);
  if (!existsSync(signature) || !meta.manifests.length) return;
  const present = spawnSync("gh", ["release", "view", "updates", "--repo", repo]);
  if (present.status !== 0) {
    check();
    run("gh", ["release", "create", "updates", "--repo", repo, "--prerelease", "--title", "Update manifests", "--notes", "Channel update manifests."]);
  }
  // All verified releases share one concurrency group, so manifest comparison and write are
  // serialized. Activation requires retiring old in-flight legacy publishers.
  for (const channel of meta.manifests) {
    const old = spawnSync("gh", ["release", "download", "updates", "--repo", repo, "-p", `${channel}.json`, "-O", "-"], { encoding: "utf8" });
    const previous = old.status === 0 ? JSON.parse(old.stdout) : null;
    if (!replaces(meta.version, previous)) continue;
    const file = join(resolve(dir, ".."), `${channel}.json`);
    writeFileSync(file, JSON.stringify(manifest({ version: meta.version, signature: readFileSync(signature, "utf8"), notes,
      url: `https://github.com/${repo}/releases/download/${meta.tag}/Nitely_${meta.version}_x64-setup.exe` }), null, 2));
    check();
    run("gh", ["release", "upload", "updates", file, "--repo", repo, "--clobber"]);
  }
}
export async function main([command, ...args] = process.argv.slice(2)) {
  if (command === "prepare") return prepare(...args);
  const meta = validateMetadata(JSON.parse(readFileSync(args[0], "utf8")));
  if (command === "dispatch") {
    for (const item of REQUIRED) run("gh", ["workflow", "run", item.workflow.split("/").at(-1), "--repo", repository(), "--ref", meta.branch,
      "-f", `channel=${meta.channel}`,
      ...(item.artifacts ? ["-f", `source=${meta.source}`, "-f", `baseline=${meta.baseline}`, "-f", `policy=${meta.policy}`] : [])]);
  } else if (command === "wait") await waitForChecks(meta, args[1]);
  else if (command === "publish") publish(meta, ...args.slice(1));
  else throw new Error("unknown verified release command");
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
